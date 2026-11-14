// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Persistent namespace credentials backed by a non-volatile blob store.

use std::collections::BTreeMap;
use thiserror::Error;
use vmcore::non_volatile_store::NonVolatileStore;
use vmcore::non_volatile_store::NonVolatileStoreError;

use self::persisted::CredentialCollection;
use self::persisted::CredentialRecord;

const STORE_VERSION: u32 = 1;
const MAX_CREDENTIALS: usize = 32;
// Encoded-size budget with headroom; the capacity test checks worst-case records fit.
const MAX_STORE_SIZE: usize = 4 * 1024;

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
pub enum CredentialStoreError {
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
    /// The update would exceed the credential count or encoded-size limit.
    #[error("credential store is full")]
    OutOfSpace,
}

/// A single-owner namespace credential store.
///
/// The caller must supply a backend that encrypts persisted credentials.
/// With `VmgsNonVolatileStore`, VMGS must be encrypted and unlocked before
/// writes. The adapter's `encrypted` flag alone is insufficient: VMGS can
/// otherwise fall back to plaintext. This wrapper cannot verify that state.
///
/// Do not open concurrent wrappers over the same backing blob: each update
/// writes the entire collection.
///
/// Updates persist before changing the in-memory state. After a storage error
/// or cancellation of an update, discard and reopen the store before further
/// use; the backing store may have changed even without an acknowledged write.
pub struct LstoraccelCredentialStore {
    store: Box<dyn NonVolatileStore>,
    state: BTreeMap<NamespaceCredentialKey, [u8; CREDENTIAL_LEN]>,
}

mod persisted {
    //! Version 1 credential-store protobuf format.
    //!
    //! Field numbers are persistent identifiers and must not be reused.

    use mesh_protobuf::Protobuf;

    /// A credential collection in format version 1.
    #[derive(Protobuf)]
    #[mesh(package = "tdisp_lstoraccel.credential_store")]
    pub(super) struct CredentialCollection {
        /// Format version. Only version 1 is currently supported.
        #[mesh(1)]
        pub(super) version: u32,
        /// Required count; detects truncation before or between complete records.
        #[mesh(2)]
        pub(super) record_count: Option<u32>,
        /// Namespace credentials, limited by [`super::MAX_CREDENTIALS`].
        #[mesh(3)]
        pub(super) records: Vec<CredentialRecord>,
    }

    /// A namespace credential record in format version 1.
    #[derive(Protobuf)]
    #[mesh(package = "tdisp_lstoraccel.credential_store")]
    pub(super) struct CredentialRecord {
        /// Exact 20-byte controller virtual serial number.
        #[mesh(1)]
        pub(super) vsn: Vec<u8>,
        /// Namespace ID within that controller.
        #[mesh(2)]
        pub(super) nsid: u32,
        /// Authorization credential, exactly [`super::CREDENTIAL_LEN`] bytes.
        #[mesh(3)]
        pub(super) credential: Vec<u8>,
    }
}

impl LstoraccelCredentialStore {
    /// Restore persisted credentials, or initialize an empty store if absent.
    pub async fn open(mut store: Box<dyn NonVolatileStore>) -> Result<Self, CredentialStoreError> {
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
    ) -> Result<(), CredentialStoreError> {
        if self.state.contains_key(key) {
            return Err(CredentialStoreError::CredentialAlreadyExists);
        }
        if self.state.len() >= MAX_CREDENTIALS {
            return Err(CredentialStoreError::OutOfSpace);
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
    ) -> Result<(), CredentialStoreError> {
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
    ) -> Result<(), CredentialStoreError> {
        let bytes = serialize_credential_store(&temp_state);
        if bytes.len() > MAX_STORE_SIZE {
            return Err(CredentialStoreError::OutOfSpace);
        }
        self.store.persist(bytes).await?;
        self.state = temp_state;
        Ok(())
    }
}

fn deserialize_credential_store(
    bytes: &[u8],
) -> Result<BTreeMap<NamespaceCredentialKey, [u8; CREDENTIAL_LEN]>, CredentialStoreError> {
    use CredentialStoreError as Error;

    if bytes.len() > MAX_STORE_SIZE {
        return Err(Error::InvalidData("credential store exceeds maximum size"));
    }
    let collection: CredentialCollection = mesh_protobuf::decode(bytes)
        .map_err(|_| Error::InvalidData("malformed credential store"))?;
    if collection.version != STORE_VERSION {
        return Err(Error::UnsupportedVersion(collection.version));
    }
    if collection.records.len() > MAX_CREDENTIALS {
        return Err(Error::InvalidData("too many credential records"));
    }
    let record_count = collection
        .record_count
        .ok_or(Error::InvalidData("missing credential record count"))?;
    if record_count as usize != collection.records.len() {
        return Err(Error::InvalidData("credential record count mismatch"));
    }

    let mut state = BTreeMap::new();
    for record in collection.records {
        let vsn = record
            .vsn
            .try_into()
            .map_err(|_| Error::InvalidData("invalid controller serial length"))?;
        let credential = record
            .credential
            .try_into()
            .map_err(|_| Error::InvalidData("invalid credential length"))?;
        let key = NamespaceCredentialKey {
            vsn,
            nsid: record.nsid,
        };
        if state.insert(key, credential).is_some() {
            return Err(Error::InvalidData("duplicate namespace credential"));
        }
    }
    Ok(state)
}

fn serialize_credential_store(
    state: &BTreeMap<NamespaceCredentialKey, [u8; CREDENTIAL_LEN]>,
) -> Vec<u8> {
    let records: Vec<_> = state
        .iter()
        .map(|(key, credential)| CredentialRecord {
            vsn: key.vsn.to_vec(),
            nsid: key.nsid,
            credential: credential.to_vec(),
        })
        .collect();
    mesh_protobuf::encode(CredentialCollection {
        version: STORE_VERSION,
        record_count: Some(records.len() as u32),
        records,
    })
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

    fn persisted_record(nsid: u32) -> CredentialRecord {
        CredentialRecord {
            vsn: vec![1; 20],
            nsid,
            credential: vec![2; CREDENTIAL_LEN],
        }
    }

    fn persisted_collection(records: Vec<CredentialRecord>) -> CredentialCollection {
        CredentialCollection {
            version: STORE_VERSION,
            record_count: Some(records.len() as u32),
            records,
        }
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

        let mut store = reopen(store).await;
        assert_eq!(store.get(&controller_a_ns1), Some(&credential_a1));
        assert!(store.get(&controller_a_ns2).is_none());
        assert_eq!(store.get(&controller_b_ns1), Some(&credential_b1));

        store.remove(&controller_a_ns1).await.unwrap();
        store.remove(&controller_b_ns1).await.unwrap();
        let store = reopen(store).await;
        assert!(store.state.is_empty());
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
                Err(CredentialStoreError::CredentialAlreadyExists)
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
        let key = |index| NamespaceCredentialKey {
            vsn: [1; 20],
            nsid: u32::MAX - (index - 1),
        };
        for index in 1..=MAX_CREDENTIALS as u32 {
            store
                .put(&key(index), &[index as u8; CREDENTIAL_LEN])
                .await
                .unwrap();
        }
        let before = store.store.restore().await.unwrap();
        assert!(before.as_ref().unwrap().len() <= MAX_STORE_SIZE);
        let mut store = reopen(store).await;
        assert!(matches!(
            store.put(&key(1), &[0; CREDENTIAL_LEN]).await,
            Err(CredentialStoreError::CredentialAlreadyExists)
        ));
        let new_key = key(MAX_CREDENTIALS as u32 + 1);
        let new_credential = [99; CREDENTIAL_LEN];
        assert!(matches!(
            store.put(&new_key, &new_credential).await,
            Err(CredentialStoreError::OutOfSpace)
        ));
        // Bypass put's record-count limit to exercise the encoded-size guard.
        let oversized_state = (1..=(MAX_STORE_SIZE / CREDENTIAL_LEN + 1) as u32)
            .map(|index| (key(index), [0; CREDENTIAL_LEN]))
            .collect();
        assert!(matches!(
            store.flush(oversized_state).await,
            Err(CredentialStoreError::OutOfSpace)
        ));
        assert_eq!(store.store.restore().await.unwrap(), before);
        assert!(store.get(&new_key).is_none());
        for index in 1..=MAX_CREDENTIALS as u32 {
            assert_eq!(store.get(&key(index)), Some(&[index as u8; CREDENTIAL_LEN]));
        }
        store.remove(&key(1)).await.unwrap();
        store.put(&new_key, &new_credential).await.unwrap();
        let store = reopen(store).await;
        assert!(store.get(&key(1)).is_none());
        assert_eq!(store.get(&new_key), Some(&new_credential));
        for index in 2..=MAX_CREDENTIALS as u32 {
            assert_eq!(store.get(&key(index)), Some(&[index as u8; CREDENTIAL_LEN]));
        }
    }

    #[test]
    fn invalid_persisted_data_is_rejected() {
        use CredentialStoreError as Error;

        for version in [0, STORE_VERSION + 1] {
            let mut collection = persisted_collection(Vec::new());
            collection.version = version;
            assert!(matches!(
                deserialize_credential_store(&mesh_protobuf::encode(collection)),
                Err(Error::UnsupportedVersion(found)) if found == version
            ));
        }

        let mut truncated = mesh_protobuf::encode(persisted_collection(vec![persisted_record(1)]));
        truncated.pop().unwrap();
        let mut invalid = vec![
            (vec![0x08], "malformed credential store"),
            (truncated, "malformed credential store"),
            (
                STORE_VERSION.to_ne_bytes().to_vec(),
                "malformed credential store",
            ),
            (
                vec![0; MAX_STORE_SIZE + 1],
                "credential store exceeds maximum size",
            ),
        ];
        for len in [0, 19, 21] {
            let mut record = persisted_record(1);
            record.vsn.resize(len, 0);
            invalid.push((
                mesh_protobuf::encode(persisted_collection(vec![record])),
                "invalid controller serial length",
            ));
        }
        for len in [0, CREDENTIAL_LEN - 1, CREDENTIAL_LEN + 1] {
            let mut record = persisted_record(1);
            record.credential.resize(len, 0);
            invalid.push((
                mesh_protobuf::encode(persisted_collection(vec![record])),
                "invalid credential length",
            ));
        }

        let duplicate = persisted_collection(vec![persisted_record(1), persisted_record(1)]);
        invalid.push((
            mesh_protobuf::encode(duplicate),
            "duplicate namespace credential",
        ));

        let excess =
            persisted_collection((0..=MAX_CREDENTIALS as u32).map(persisted_record).collect());
        let bytes = mesh_protobuf::encode(excess);
        assert!(bytes.len() <= MAX_STORE_SIZE);
        invalid.push((bytes, "too many credential records"));

        for (bytes, expected) in invalid {
            assert!(
                matches!(
                    deserialize_credential_store(&bytes),
                    Err(Error::InvalidData(message)) if message == expected
                ),
                "expected {expected}"
            );
        }
    }

    #[test]
    fn record_count_must_match_the_collection() {
        // A populated blob truncated immediately after its version field.
        assert!(matches!(
            deserialize_credential_store(&[0x08, 0x01]),
            Err(CredentialStoreError::InvalidData(
                "missing credential record count"
            ))
        ));

        let empty = mesh_protobuf::encode(persisted_collection(Vec::new()));
        assert!(deserialize_credential_store(&empty).unwrap().is_empty());

        let mut collection = persisted_collection(vec![persisted_record(1)]);
        collection.record_count = Some(0);
        assert!(matches!(
            deserialize_credential_store(&mesh_protobuf::encode(collection)),
            Err(CredentialStoreError::InvalidData(
                "credential record count mismatch"
            ))
        ));

        let mut collection = persisted_collection(vec![persisted_record(1)]);
        collection.record_count = Some(2);
        let prefix = mesh_protobuf::encode(collection);
        let complete = mesh_protobuf::encode(persisted_collection(vec![
            persisted_record(1),
            persisted_record(2),
        ]));
        assert_eq!(&complete[..prefix.len()], prefix);
        assert!(matches!(
            deserialize_credential_store(&complete[..prefix.len()]),
            Err(CredentialStoreError::InvalidData(
                "credential record count mismatch"
            ))
        ));
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
