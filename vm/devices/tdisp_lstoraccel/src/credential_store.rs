// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Persistent namespace credentials backed by a non-volatile blob store.

use std::collections::BTreeMap;
use thiserror::Error;
use vmcore::non_volatile_store::NonVolatileStore;
use vmcore::non_volatile_store::NonVolatileStoreError;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const STORE_VERSION: u32 = 1;
const MAX_CREDENTIALS: usize = 32;
const MAX_STORE_SIZE: usize =
    size_of::<CredentialStoreHeader>() + MAX_CREDENTIALS * size_of::<CredentialRecord>();

/// Authorization credential size in bytes.
pub const CREDENTIAL_LEN: usize = 32;

/// The controller serial number and namespace ID identifying a credential.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct NamespaceCredentialKey {
    /// The controller's virtual serial number.
    pub vsn: [u8; 20],
    /// The namespace ID within the controller.
    pub nsid: u32,
}

/// An error accessing or updating namespace credentials.
#[derive(Debug, Error)]
pub enum LstoraccelCredentialStoreError {
    /// The backing store could not be read or written.
    #[error("could not access credential storage")]
    Storage(#[from] NonVolatileStoreError),
    /// The persisted format version is not supported.
    #[error("unsupported credential store version {0}")]
    UnsupportedVersion(u32),
    /// The persisted data is malformed.
    #[error("invalid credential store: {0}")]
    InvalidData(&'static str),
    /// The namespace already has a credential.
    #[error("a credential already exists for this namespace")]
    CredentialAlreadyExists,
    /// The store has reached its credential capacity.
    #[error("credential store is full")]
    OutOfSpace,
}

/// A single-owner namespace credential store.
///
/// The caller supplies a backend configured for encrypted persistence. Do not
/// open concurrent wrappers over the same backing blob: each update writes the
/// entire collection.
///
/// Updates persist before changing the in-memory state. After a storage error
/// or cancellation of an update, discard and reopen the store before further
/// use; the backing store may have changed even without an acknowledged write.
pub struct LstoraccelCredentialStore {
    store: Box<dyn NonVolatileStore>,
    state: BTreeMap<NamespaceCredentialKey, [u8; CREDENTIAL_LEN]>,
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
struct CredentialStoreHeader {
    version: u32,
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
struct CredentialRecord {
    vsn: [u8; 20],
    nsid: u32,
    credential: [u8; CREDENTIAL_LEN],
}

static_assertions::const_assert_eq!(size_of::<CredentialStoreHeader>(), 4);
static_assertions::const_assert_eq!(size_of::<CredentialRecord>(), 56);

impl LstoraccelCredentialStore {
    /// Restore persisted credentials, or initialize an empty store if absent.
    pub async fn open(
        mut store: Box<dyn NonVolatileStore>,
    ) -> Result<Self, LstoraccelCredentialStoreError> {
        let state = match store.restore().await? {
            Some(bytes) => deserialize_credential_store(&bytes)?,
            None => BTreeMap::new(),
        };
        Ok(Self { store, state })
    }

    /// Returns the credential for an exact VSN/NSID match, or `None` if absent.
    pub fn get(&self, key: &NamespaceCredentialKey) -> Option<&[u8; CREDENTIAL_LEN]> {
        self.state.get(key)
    }

    /// Persist a credential for a namespace that has no stored credential.
    pub async fn put(
        &mut self,
        key: &NamespaceCredentialKey,
        credential: &[u8; CREDENTIAL_LEN],
    ) -> Result<(), LstoraccelCredentialStoreError> {
        if self.state.contains_key(key) {
            return Err(LstoraccelCredentialStoreError::CredentialAlreadyExists);
        }
        if self.state.len() >= MAX_CREDENTIALS {
            return Err(LstoraccelCredentialStoreError::OutOfSpace);
        }
        let mut temp_state = self.state.clone();
        temp_state.insert(*key, *credential);
        self.flush(temp_state).await
    }

    /// Remove a credential after the caller confirms namespace deletion.
    ///
    /// Removing an absent credential succeeds without writing the backing store.
    pub async fn remove(
        &mut self,
        key: &NamespaceCredentialKey,
    ) -> Result<(), LstoraccelCredentialStoreError> {
        if !self.state.contains_key(key) {
            return Ok(());
        }
        let mut temp_state = self.state.clone();
        temp_state.remove(key);
        self.flush(temp_state).await
    }

    async fn flush(
        &mut self,
        temp_state: BTreeMap<NamespaceCredentialKey, [u8; CREDENTIAL_LEN]>,
    ) -> Result<(), LstoraccelCredentialStoreError> {
        self.store
            .persist(serialize_credential_store(&temp_state))
            .await?;
        self.state = temp_state;
        Ok(())
    }
}

fn deserialize_credential_store(
    bytes: &[u8],
) -> Result<BTreeMap<NamespaceCredentialKey, [u8; CREDENTIAL_LEN]>, LstoraccelCredentialStoreError>
{
    use LstoraccelCredentialStoreError as Error;

    if bytes.len() > MAX_STORE_SIZE {
        return Err(Error::InvalidData("credential store exceeds maximum size"));
    }
    let (header, mut bytes) = CredentialStoreHeader::read_from_prefix(bytes)
        .map_err(|_| Error::InvalidData("truncated store header"))?;
    if header.version != STORE_VERSION {
        return Err(Error::UnsupportedVersion(header.version));
    }
    let mut state = BTreeMap::new();
    while !bytes.is_empty() {
        let (record, rest) = CredentialRecord::read_from_prefix(bytes)
            .map_err(|_| Error::InvalidData("truncated credential record"))?;
        let key = NamespaceCredentialKey {
            vsn: record.vsn,
            nsid: record.nsid,
        };
        if state.insert(key, record.credential).is_some() {
            return Err(Error::InvalidData("duplicate namespace credential"));
        }
        bytes = rest;
    }
    Ok(state)
}

fn serialize_credential_store(
    state: &BTreeMap<NamespaceCredentialKey, [u8; CREDENTIAL_LEN]>,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(
        size_of::<CredentialStoreHeader>() + state.len() * size_of::<CredentialRecord>(),
    );
    bytes.extend_from_slice(
        CredentialStoreHeader {
            version: STORE_VERSION,
        }
        .as_bytes(),
    );
    for (key, credential) in state {
        bytes.extend_from_slice(
            CredentialRecord {
                vsn: key.vsn,
                nsid: key.nsid,
                credential: *credential,
            }
            .as_bytes(),
        );
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;
    use vmcore::non_volatile_store::EphemeralNonVolatileStore;
    use vmgs::EncryptionAlgorithm;
    use vmgs::FileId;
    use vmgs::Vmgs;
    use vmgs_broker::non_volatile_store::VmgsNonVolatileStore;
    use vmgs_broker::spawn_vmgs_broker;

    async fn new_store() -> LstoraccelCredentialStore {
        LstoraccelCredentialStore::open(EphemeralNonVolatileStore::new_boxed())
            .await
            .unwrap()
    }

    async fn reopen(store: LstoraccelCredentialStore) -> LstoraccelCredentialStore {
        LstoraccelCredentialStore::open(store.store).await.unwrap()
    }

    async fn open_store_with_persisted_bytes(
        bytes: Vec<u8>,
    ) -> Result<LstoraccelCredentialStore, LstoraccelCredentialStoreError> {
        let mut store = EphemeralNonVolatileStore::new_boxed();
        store.persist(bytes).await.unwrap();
        LstoraccelCredentialStore::open(store).await
    }

    #[async_test]
    async fn credentials_are_keyed_by_vsn_and_nsid() {
        let controller_a_ns1 = NamespaceCredentialKey {
            vsn: [0; 20],
            nsid: 1,
        };
        let controller_a_ns2 = NamespaceCredentialKey {
            vsn: [0; 20],
            nsid: 2,
        };
        let controller_b_ns1 = NamespaceCredentialKey {
            vsn: [2; 20],
            nsid: 1,
        };
        let credential_a1 = [1; CREDENTIAL_LEN];
        let credential_a2 = [2; CREDENTIAL_LEN];
        let credential_b1 = [3; CREDENTIAL_LEN];
        let mut store = new_store().await;
        assert!(store.get(&controller_a_ns1).is_none());
        store.put(&controller_b_ns1, &credential_b1).await.unwrap();
        store.put(&controller_a_ns2, &credential_a2).await.unwrap();
        store.put(&controller_a_ns1, &credential_a1).await.unwrap();
        assert_eq!(store.get(&controller_a_ns1), Some(&credential_a1));
        assert_eq!(store.get(&controller_a_ns2), Some(&credential_a2));
        assert_eq!(store.get(&controller_b_ns1), Some(&credential_b1));
        store.remove(&controller_a_ns2).await.unwrap();
        store.remove(&controller_a_ns2).await.unwrap();

        let store = reopen(store).await;
        assert_eq!(store.get(&controller_a_ns1), Some(&credential_a1));
        assert!(store.get(&controller_a_ns2).is_none());
        assert_eq!(store.get(&controller_b_ns1), Some(&credential_b1));
    }

    #[async_test]
    async fn existing_credentials_cannot_be_replaced() {
        let key = NamespaceCredentialKey {
            vsn: [1; 20],
            nsid: 1,
        };
        let credential = [1; CREDENTIAL_LEN];
        let replacement = [2; CREDENTIAL_LEN];
        let mut store = new_store().await;
        store.put(&key, &credential).await.unwrap();
        let before = store.store.restore().await.unwrap();
        for value in [&credential, &replacement] {
            assert!(matches!(
                store.put(&key, value).await,
                Err(LstoraccelCredentialStoreError::CredentialAlreadyExists)
            ));
        }
        assert_eq!(store.get(&key), Some(&credential));
        assert_eq!(store.store.restore().await.unwrap(), before);
        let store = reopen(store).await;
        assert_eq!(store.get(&key), Some(&credential));
    }

    #[async_test]
    async fn capacity_limit_preserves_existing_credentials() {
        let mut store = new_store().await;
        let key = |nsid| NamespaceCredentialKey { vsn: [1; 20], nsid };
        for nsid in 1..=MAX_CREDENTIALS as u32 {
            store
                .put(&key(nsid), &[nsid as u8; CREDENTIAL_LEN])
                .await
                .unwrap();
        }
        let before = store.store.restore().await.unwrap();
        assert!(matches!(
            store.put(&key(1), &[0; CREDENTIAL_LEN]).await,
            Err(LstoraccelCredentialStoreError::CredentialAlreadyExists)
        ));
        let new_key = key(MAX_CREDENTIALS as u32 + 1);
        let new_credential = [99; CREDENTIAL_LEN];
        assert!(matches!(
            store.put(&new_key, &new_credential).await,
            Err(LstoraccelCredentialStoreError::OutOfSpace)
        ));
        assert_eq!(store.store.restore().await.unwrap(), before);
        assert!(store.get(&new_key).is_none());
        for nsid in 1..=MAX_CREDENTIALS as u32 {
            assert_eq!(store.get(&key(nsid)), Some(&[nsid as u8; CREDENTIAL_LEN]));
        }
        store.remove(&key(1)).await.unwrap();
        store.put(&new_key, &new_credential).await.unwrap();
        let store = reopen(store).await;
        assert!(store.get(&key(1)).is_none());
        assert_eq!(store.get(&new_key), Some(&new_credential));
        for nsid in 2..=MAX_CREDENTIALS as u32 {
            assert_eq!(store.get(&key(nsid)), Some(&[nsid as u8; CREDENTIAL_LEN]));
        }
    }

    #[async_test]
    async fn malformed_data_is_not_an_empty_store() {
        use LstoraccelCredentialStoreError as Error;

        for len in 0..size_of::<CredentialStoreHeader>() {
            assert!(matches!(
                open_store_with_persisted_bytes(vec![0; len]).await,
                Err(Error::InvalidData("truncated store header"))
            ));
        }
        let unsupported = CredentialStoreHeader {
            version: STORE_VERSION + 1,
        }
        .as_bytes()
        .to_vec();
        assert!(matches!(
            open_store_with_persisted_bytes(unsupported).await,
            Err(Error::UnsupportedVersion(version)) if version == STORE_VERSION + 1
        ));
        let header = CredentialStoreHeader {
            version: STORE_VERSION,
        }
        .as_bytes()
        .to_vec();
        for len in [
            1,
            size_of::<CredentialRecord>() - 1,
            size_of::<CredentialRecord>() + 1,
        ] {
            let mut bytes = header.clone();
            bytes.resize(header.len() + len, 0);
            assert!(matches!(
                open_store_with_persisted_bytes(bytes).await,
                Err(Error::InvalidData("truncated credential record"))
            ));
        }
        let mut oversized = header.clone();
        oversized.resize(MAX_STORE_SIZE + 1, 0);
        assert!(matches!(
            open_store_with_persisted_bytes(oversized).await,
            Err(Error::InvalidData("credential store exceeds maximum size"))
        ));
        let store = open_store_with_persisted_bytes(header).await.unwrap();
        assert!(store.state.is_empty());
    }

    #[async_test]
    async fn credentials_survive_encrypted_vmgs_reopen(driver: DefaultDriver) {
        let disk = disklayer_ram::ram_disk(4 * 1024 * 1024, false).unwrap();
        let encryption_key = [0x11; 32];
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
        vmgs.update_encryption_key(&encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        let (client, task) = spawn_vmgs_broker(driver.clone(), vmgs);
        let backend =
            VmgsNonVolatileStore::new(client.clone(), FileId::LSTORACCEL_CREDENTIAL_STORE, true)
                .unwrap();
        let mut store = LstoraccelCredentialStore::open(Box::new(backend))
            .await
            .unwrap();
        let key = NamespaceCredentialKey {
            vsn: [1; 20],
            nsid: 7,
        };
        let credential = [0x22; CREDENTIAL_LEN];
        store.put(&key, &credential).await.unwrap();
        assert!(
            client
                .get_file_info(FileId::LSTORACCEL_CREDENTIAL_STORE)
                .await
                .unwrap()
                .encrypted
        );
        drop(store);
        drop(client);
        task.await;

        let mut vmgs = Vmgs::open(disk, None).await.unwrap();
        assert!(vmgs.unlock_with_encryption_key(&[0x33; 32]).await.is_err());
        vmgs.unlock_with_encryption_key(&encryption_key)
            .await
            .unwrap();
        let (client, task) = spawn_vmgs_broker(driver, vmgs);
        let backend =
            VmgsNonVolatileStore::new(client, FileId::LSTORACCEL_CREDENTIAL_STORE, true).unwrap();
        let store = LstoraccelCredentialStore::open(Box::new(backend))
            .await
            .unwrap();
        assert_eq!(store.get(&key), Some(&credential));
        drop(store);
        task.await;
    }
}
