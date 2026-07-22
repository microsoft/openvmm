// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HCL-compatible UEFI nvram variable storage format.
//!
//! Stores Nvram variables as a _packed_ byte-buffer of structs + associated
//! variable length data, in the same format as the earlier Microsoft HCL
//! versions.
//!
//! # A brief comment about the data representation
//!
//! Because variables are stored in the buffer back-to-back with no padding, the
//! UTF-16 encoded `name` field is _not_ guaranteed to be properly aligned,
//! which means it's invalid to reference it as a `&[u16]`, or any similar
//! wrapper type (e.g: `widestring::U16CStr`).

#![forbid(unsafe_code)]

pub mod storage_backend;

use core::mem::size_of;
use core::mem::size_of_val;
use cvm_tracing::CVM_ALLOWED;
use cvm_tracing::CVM_CONFIDENTIAL;
use guid::Guid;
use hcl_compat_uefi_nvram_resources::HclCompatNvramQuirks;
use std::collections::BTreeSet;
use storage_backend::StorageBackend;
use ucs2::Ucs2LeSlice;
use uefi_nvram_specvars::signature_list::ParseError as SignatureListParseError;
use uefi_nvram_specvars::signature_list::ParseSignatureList;
use uefi_nvram_specvars::signature_list::ParseSignatureLists;
use uefi_nvram_storage::EFI_TIME;
use uefi_nvram_storage::NextVariable;
use uefi_nvram_storage::NvramStorage;
use uefi_nvram_storage::NvramStorageError;
use uefi_nvram_storage::in_memory;
use uefi_specs::uefi::nvram::EFI_VARIABLE_AUTHENTICATION_2;
use uefi_specs::uefi::nvram::vars;
use uefi_specs::uefi::signing::EFI_CERT_TYPE_PKCS7_GUID;
use uefi_specs::uefi::signing::WIN_CERT_TYPE_EFI_GUID;
use uefi_specs::uefi::signing::WIN_CERTIFICATE_UEFI_GUID;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const EFI_MAX_VARIABLE_NAME_SIZE: usize = 2 * 1024;
const EFI_MAX_VARIABLE_DATA_SIZE: usize = 32 * 1024;

// Max size allows two re-sizings, max size of 128K
// TODO: how big required for secure boot with db/dbx?
const INITIAL_NVRAM_SIZE: usize = 32768;
const MAXIMUM_NVRAM_SIZE: usize = INITIAL_NVRAM_SIZE * 4;
const WIN_CERT_REVISION_2_0: u16 = 0x0200;

/// Signature identity for baseline telemetry; `SignatureOwner` is intentionally ignored.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SignatureValue {
    X509(Vec<u8>),
    Sha256(Vec<u8>),
}

type SignatureSet = BTreeSet<SignatureValue>;

/// Base Secure Boot template variable contents expected to be present in loaded NVRAM.
#[derive(Clone, Debug)]
pub struct BaseSecureBootTemplateVariables {
    pk: Vec<u8>,
    kek: Vec<u8>,
    db: Vec<u8>,
    dbx: Vec<u8>,
}

impl BaseSecureBootTemplateVariables {
    /// Create a new base Secure Boot template variable set.
    pub fn new(pk: Vec<u8>, kek: Vec<u8>, db: Vec<u8>, dbx: Vec<u8>) -> Self {
        Self { pk, kek, db, dbx }
    }

    /// Return whether there are no base Secure Boot template variables to track.
    fn is_empty(&self) -> bool {
        self.pk.is_empty() && self.kek.is_empty() && self.db.is_empty() && self.dbx.is_empty()
    }
}

mod format {
    use super::*;
    use open_enum::open_enum;
    use static_assertions::const_assert_eq;

    open_enum! {
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        pub enum NvramHeaderType: u32 {
            VARIABLE = 0,
        }
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct NvramHeader {
        pub header_type: NvramHeaderType,
        pub length: u32, // Total length of the variable, in bytes. Includes the header.
    }

    const_assert_eq!(8, size_of::<NvramHeader>());

    #[repr(C)]
    #[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct NvramVariable {
        pub header: NvramHeader, // Set to type NvramVariable
        pub attributes: u32,
        pub timestamp: EFI_TIME, // Only used by authenticated variables
        pub vendor: Guid,
        pub name_bytes: u16, // max name size of 2K, in _bytes_ not number of characters
        pub data_bytes: u16, // max data size of 32K
                             // std::uint16_t Name[];
                             // std::uint8_t Data[]; // Follows after Name.
    }
    const_assert_eq!(48, size_of::<NvramVariable>());
}

/// Stores Nvram variables in files as a _packed_ byte-buffer of structs +
/// associated variable length data.
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub struct HclCompatNvram<S> {
    quirks: HclCompatNvramQuirks,

    #[cfg_attr(feature = "inspect", inspect(skip))]
    storage: S,

    in_memory: in_memory::InMemoryNvram,

    // reuse the same allocation for the nvram_buf, trading off steady-state
    // memory usage for a more consistent (albeit larger) memory footprint, and
    // reduced allocator pressure
    #[cfg_attr(feature = "inspect", inspect(skip))] // internal bookkeeping - not worth inspecting
    nvram_buf: Vec<u8>,

    // whether the NVRAM has been loaded, either from storage or saved state
    loaded: bool,

    #[cfg_attr(feature = "inspect", inspect(skip))]
    base_secure_boot_template_variables: Option<BaseSecureBootTemplateVariables>,
}

impl<S: StorageBackend> HclCompatNvram<S> {
    /// Create a new [`HclCompatNvram`]
    pub fn new(storage: S, quirks: Option<HclCompatNvramQuirks>) -> Self {
        Self {
            quirks: quirks.unwrap_or(HclCompatNvramQuirks {
                skip_corrupt_vars_with_missing_null_term: false,
            }),

            storage,

            in_memory: in_memory::InMemoryNvram::new(),

            nvram_buf: Vec::new(),

            loaded: false,

            base_secure_boot_template_variables: None,
        }
    }

    /// Store the base Secure Boot template variables for later observation.
    pub fn with_base_secure_boot_template_variables(
        mut self,
        template: BaseSecureBootTemplateVariables,
    ) -> Self {
        if !template.is_empty() {
            self.base_secure_boot_template_variables = Some(template);
        }
        self
    }

    async fn lazy_load_from_storage(&mut self) -> Result<(), NvramStorageError> {
        let res = self.lazy_load_from_storage_inner().await;
        if let Err(e) = &res {
            tracing::error!(CVM_ALLOWED, "storage contains corrupt nvram state");
            tracing::error!(
                CVM_CONFIDENTIAL,
                error = e as &dyn std::error::Error,
                "storage contains corrupt nvram state"
            );
        }
        res
    }

    async fn lazy_load_from_storage_inner(&mut self) -> Result<(), NvramStorageError> {
        if self.loaded {
            return Ok(());
        }

        let nvram_buf = self
            .storage
            .restore()
            .await
            .map_err(|e| NvramStorageError::Load(e.into()))?
            .unwrap_or_default();
        let loaded_existing_state = !nvram_buf.is_empty();

        if nvram_buf.len() > MAXIMUM_NVRAM_SIZE {
            return Err(NvramStorageError::Load(
                format!(
                    "Existing nvram state exceeds MAXIMUM_NVRAM_SIZE ({} > {})",
                    nvram_buf.len(),
                    MAXIMUM_NVRAM_SIZE
                )
                .into(),
            ));
        }

        // load state into memory
        self.in_memory.clear();
        self.nvram_buf = nvram_buf;
        let mut buf = self.nvram_buf.as_slice();
        // TODO: zerocopy: error propagation (https://github.com/microsoft/openvmm/issues/759)
        while let Ok((header, _)) = format::NvramHeader::read_from_prefix(buf) {
            if buf.len() < header.length as usize {
                return Err(NvramStorageError::Load(
                    format!(
                        "unexpected EOF. expected at least {} more bytes, but only found {}",
                        header.length,
                        buf.len()
                    )
                    .into(),
                ));
            }

            let entry_buf = {
                let (entry_buf, remaining) = buf.split_at(header.length as usize);
                buf = remaining;
                entry_buf
            };

            match header.header_type {
                format::NvramHeaderType::VARIABLE => {}
                _ => {
                    return Err(NvramStorageError::Load(
                        format!("unknown header type: {:?}", header.header_type).into(),
                    ));
                }
            }

            // validation check above ensures that at this point, entry_buf
            // corresponds to a VARIABLE entry

            let (var_header, var_name, var_data) = {
                // TODO: zerocopy: error propagation (https://github.com/microsoft/openvmm/issues/759)
                // TODO: zerocopy: manual fix - review carefully! (https://github.com/microsoft/openvmm/issues/759)
                let (var_header, var_length_data) =
                    format::NvramVariable::read_from_prefix(entry_buf)
                        .map_err(|_| NvramStorageError::Load("variable entry too short".into()))?;

                if var_length_data.len()
                    != var_header.name_bytes as usize + var_header.data_bytes as usize
                {
                    return Err(NvramStorageError::Load(
                        "mismatch between header length and variable data size".into(),
                    ));
                }

                let (var_name, var_data) = var_length_data.split_at(var_header.name_bytes as usize);

                (var_header, var_name, var_data)
            };

            if var_name.len() > EFI_MAX_VARIABLE_NAME_SIZE {
                return Err(NvramStorageError::Load(
                    format!(
                        "variable name too big. {} > {}",
                        var_name.len(),
                        EFI_MAX_VARIABLE_NAME_SIZE
                    )
                    .into(),
                ));
            }

            if var_data.len() > EFI_MAX_VARIABLE_DATA_SIZE {
                return Err(NvramStorageError::Load(
                    format!(
                        "variable data too big. {} > {}",
                        var_data.len(),
                        EFI_MAX_VARIABLE_DATA_SIZE
                    )
                    .into(),
                ));
            }

            let name = match Ucs2LeSlice::from_slice_with_nul(var_name) {
                Ok(name) => name,
                Err(e) => {
                    if self.quirks.skip_corrupt_vars_with_missing_null_term {
                        let var = {
                            let mut var = var_name.to_vec();
                            var.push(0);
                            var.push(0);
                            ucs2::Ucs2LeVec::from_vec_with_nul(var)
                        };
                        tracing::warn!(
                            CVM_ALLOWED,
                            "skipping corrupt nvram var (missing null term)"
                        );
                        tracing::warn!(
                            CVM_CONFIDENTIAL,
                            ?var,
                            "skipping corrupt nvram var (missing null term)"
                        );
                        continue;
                    } else {
                        return Err(NvramStorageError::Load(e.into()));
                    }
                }
            };

            self.in_memory
                .set_variable(
                    name,
                    var_header.vendor,
                    var_header.attributes,
                    var_data.to_vec(),
                    var_header.timestamp,
                )
                .await?;
        }

        if !buf.is_empty() {
            return Err(NvramStorageError::Load(
                "existing nvram state contains excess data".into(),
            ));
        }

        self.loaded = true;
        if loaded_existing_state {
            self.report_secure_boot_base_template_presence();
        }
        Ok(())
    }

    /// Report whether each base Secure Boot template variable is present in loaded NVRAM.
    fn report_secure_boot_base_template_presence(&self) {
        let Some(template) = &self.base_secure_boot_template_variables else {
            return;
        };

        // TODO: Determine whether custom keys can be appended to PK before
        // requiring an exact PK match instead of baseline set membership.
        for (variable, (vendor, name), base_template_variable) in [
            ("PK", vars::PK(), template.pk.as_slice()),
            ("KEK", vars::KEK(), template.kek.as_slice()),
            ("db", vars::DB(), template.db.as_slice()),
            ("dbx", vars::DBX(), template.dbx.as_slice()),
        ] {
            // Load the variable from NVRAM.
            let loaded_variable = match self
                .in_memory
                .iter()
                .find(|entry| entry.vendor == vendor && entry.name.as_bytes() == name.as_bytes())
                .map(|entry| entry.data)
            {
                Some(loaded_variable) if !loaded_variable.is_empty() => loaded_variable,
                loaded_variable => {
                    tracing::warn!(
                        CVM_ALLOWED,
                        variable,
                        loaded_variable_bytes = loaded_variable.map_or(0, |data| data.len()),
                        "base secure boot template variable is missing in NVRAM"
                    );
                    continue;
                }
            };

            // Parse the loaded variable into a set of signatures.
            let loaded_variable_bytes = loaded_variable.len();
            let loaded_signatures = match collect_signature_set(loaded_variable) {
                Ok(signatures) => signatures,
                Err(error) => {
                    tracing::warn!(
                        CVM_CONFIDENTIAL,
                        variable,
                        error = &error as &dyn std::error::Error,
                        "failed to parse loaded secure boot variable"
                    );
                    continue;
                }
            };
            let loaded_entries = loaded_signatures.len();

            // Parse the base template variable into a set of signatures.
            let base_template_bytes = base_template_variable.len();
            let base_template_signatures = match collect_signature_set(base_template_variable) {
                Ok(signatures) if !signatures.is_empty() => signatures,
                Ok(_) => {
                    tracing::warn!(
                        CVM_ALLOWED,
                        variable,
                        base_template_bytes,
                        "base secure boot template variable contains no signatures"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        CVM_CONFIDENTIAL,
                        variable,
                        error = &error as &dyn std::error::Error,
                        "failed to parse base secure boot template variable"
                    );
                    continue;
                }
            };
            let base_template_entries = base_template_signatures.len();

            // Count how many base template signatures are missing from the loaded variable.
            let missing_entries = base_template_signatures
                .difference(&loaded_signatures)
                .count();

            if missing_entries == 0 {
                tracing::info!(
                    CVM_ALLOWED,
                    variable,
                    base_template_entries,
                    loaded_entries,
                    missing_entries,
                    base_template_bytes,
                    loaded_variable_bytes,
                    "base secure boot template variable is present"
                );
            } else {
                tracing::warn!(
                    CVM_ALLOWED,
                    variable,
                    base_template_entries,
                    loaded_entries,
                    missing_entries,
                    base_template_bytes,
                    loaded_variable_bytes,
                    "base secure boot template variable is missing"
                );
            }
        }
    }

    /// Dump in-memory nvram to the underlying storage device.
    async fn flush_storage(&mut self) -> Result<(), NvramStorageError> {
        self.nvram_buf.clear();

        for in_memory::VariableEntry {
            vendor,
            name,
            data,
            timestamp,
            attr,
        } in self.in_memory.iter()
        {
            self.nvram_buf.extend_from_slice(
                format::NvramVariable {
                    header: format::NvramHeader {
                        header_type: format::NvramHeaderType::VARIABLE,
                        length: (size_of::<format::NvramVariable>()
                            + name.as_bytes().len()
                            + data.len()) as u32,
                    },
                    attributes: attr,
                    timestamp,
                    vendor,
                    name_bytes: name.as_bytes().len() as u16,
                    data_bytes: data.len() as u16,
                }
                .as_bytes(),
            );
            self.nvram_buf.extend_from_slice(name.as_bytes());
            self.nvram_buf.extend_from_slice(data);
        }

        // callers make sure that any operations that add/append to vars will
        // not result in file size exceeding MAXIMUM_NVRAM_SIZE
        assert!(self.nvram_buf.len() < MAXIMUM_NVRAM_SIZE);

        self.storage
            .persist(self.nvram_buf.clone())
            .await
            .map_err(|e| NvramStorageError::Commit(e.into()))?;

        Ok(())
    }

    /// Iterate over the NVRAM entries. This function asynchronously loads the
    /// NVRAM contents into memory from the backing storage if necessary.
    pub async fn iter(
        &mut self,
    ) -> Result<impl Iterator<Item = in_memory::VariableEntry<'_>>, NvramStorageError> {
        self.lazy_load_from_storage().await?;
        Ok(self.in_memory.iter())
    }
}

#[async_trait::async_trait]
impl<S: StorageBackend> NvramStorage for HclCompatNvram<S> {
    async fn get_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
    ) -> Result<Option<(u32, Vec<u8>, EFI_TIME)>, NvramStorageError> {
        self.lazy_load_from_storage().await?;

        if name.as_bytes().len() > EFI_MAX_VARIABLE_NAME_SIZE {
            return Err(NvramStorageError::VariableNameTooLong);
        }

        self.in_memory.get_variable(name, vendor).await
    }

    async fn set_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
        attr: u32,
        data: Vec<u8>,
        timestamp: EFI_TIME,
    ) -> Result<(), NvramStorageError> {
        self.lazy_load_from_storage().await?;

        if name.as_bytes().len() > EFI_MAX_VARIABLE_NAME_SIZE {
            return Err(NvramStorageError::VariableNameTooLong);
        }

        if data.len() > EFI_MAX_VARIABLE_DATA_SIZE {
            return Err(NvramStorageError::VariableDataTooLong);
        }

        // don't overshoot MAXIMUM_NVRAM_SIZE
        {
            let new_file_size = match self.in_memory.get_variable(name, vendor).await? {
                Some((_, existing_data, _)) => {
                    self.nvram_buf.len() - existing_data.len() + data.len()
                }
                None => {
                    self.nvram_buf.len()
                        + name.as_bytes().len()
                        + data.len()
                        + size_of::<format::NvramVariable>()
                }
            };

            if new_file_size > MAXIMUM_NVRAM_SIZE {
                return Err(NvramStorageError::OutOfSpace);
            }
        }

        self.in_memory
            .set_variable(name, vendor, attr, data, timestamp)
            .await?;
        self.flush_storage().await?;

        Ok(())
    }

    async fn append_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
        data: Vec<u8>,
        timestamp: EFI_TIME,
    ) -> Result<bool, NvramStorageError> {
        self.lazy_load_from_storage().await?;

        if name.as_bytes().len() > EFI_MAX_VARIABLE_NAME_SIZE {
            return Err(NvramStorageError::VariableNameTooLong);
        }

        if let Some((_, existing_data, _)) = self.in_memory.get_variable(name, vendor).await? {
            if existing_data.len() + data.len() > EFI_MAX_VARIABLE_DATA_SIZE {
                return Err(NvramStorageError::VariableDataTooLong);
            }

            let new_file_size = self.nvram_buf.len() + data.len();

            if new_file_size > MAXIMUM_NVRAM_SIZE {
                return Err(NvramStorageError::OutOfSpace);
            }
        }

        let found = self
            .in_memory
            .append_variable(name, vendor, data, timestamp)
            .await?;
        self.flush_storage().await?;

        Ok(found)
    }

    async fn remove_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
    ) -> Result<bool, NvramStorageError> {
        self.lazy_load_from_storage().await?;

        if name.as_bytes().len() > EFI_MAX_VARIABLE_NAME_SIZE {
            return Err(NvramStorageError::VariableNameTooLong);
        }

        let removed = self.in_memory.remove_variable(name, vendor).await?;
        self.flush_storage().await?;

        Ok(removed)
    }

    async fn next_variable(
        &mut self,
        name_vendor: Option<(&Ucs2LeSlice, Guid)>,
    ) -> Result<NextVariable, NvramStorageError> {
        self.lazy_load_from_storage().await?;

        if let Some((name, _)) = name_vendor {
            if name.as_bytes().len() > EFI_MAX_VARIABLE_NAME_SIZE {
                return Err(NvramStorageError::VariableNameTooLong);
            }
        }

        self.in_memory.next_variable(name_vendor).await
    }

    fn after_custom_vars_injected(&self) {
        HclCompatNvram::report_secure_boot_base_template_presence(self)
    }
}

/// Parse a serialized Secure Boot variable into a comparable set of signatures.
fn collect_signature_set(data: &[u8]) -> Result<SignatureSet, SignatureListParseError> {
    let mut signatures = BTreeSet::new();

    for list in ParseSignatureLists::new(signature_list_payload(data)) {
        match list? {
            ParseSignatureList::X509(certs) => {
                for cert in certs {
                    let cert = cert?;
                    signatures.insert(SignatureValue::X509(cert.data.0.as_ref().to_vec()));
                }
            }
            ParseSignatureList::Sha256(digests) => {
                for digest in digests {
                    let digest = digest?;
                    signatures.insert(SignatureValue::Sha256(digest.data.0.as_ref().to_vec()));
                }
            }
        }
    }

    Ok(signatures)
}

/// Return the `EFI_SIGNATURE_LIST` payload, skipping a valid auth header if present.
fn signature_list_payload(data: &[u8]) -> &[u8] {
    let Ok((auth, _)) = EFI_VARIABLE_AUTHENTICATION_2::read_from_prefix(data) else {
        return data;
    };

    if auth.auth_info.header.revision != WIN_CERT_REVISION_2_0
        || auth.auth_info.header.certificate_type != WIN_CERT_TYPE_EFI_GUID
        || auth.auth_info.cert_type != EFI_CERT_TYPE_PKCS7_GUID
    {
        return data;
    }

    let cert_len = auth.auth_info.header.length as usize;
    if cert_len < size_of::<WIN_CERTIFICATE_UEFI_GUID>() {
        return data;
    }

    let Some(auth_len) = size_of_val(&auth.timestamp).checked_add(cert_len) else {
        return data;
    };
    data.get(auth_len..).unwrap_or(data)
}

#[cfg(feature = "save_restore")]
mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    impl<S: StorageBackend> SaveRestore for HclCompatNvram<S> {
        type SavedState = <in_memory::InMemoryNvram as SaveRestore>::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            self.in_memory.save()
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            if state.nvram.is_some() {
                self.in_memory.restore(state)?;
                self.loaded = true;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::storage_backend::StorageBackend;
    use super::storage_backend::StorageBackendError;
    use super::*;
    use pal_async::async_test;
    use std::borrow::Cow;
    use ucs2::Ucs2LeVec;
    use uefi_nvram_specvars::signature_list::SignatureData;
    use uefi_nvram_specvars::signature_list::SignatureList;
    use uefi_nvram_storage::in_memory::impl_agnostic_tests;
    use wchar::wchz;

    const TEST_OWNER: Guid = Guid {
        data1: 1,
        data2: 0,
        data3: 0,
        data4: [0; 8],
    };

    fn x509_variable(owner: Guid, certs: &[&'static [u8]]) -> Vec<u8> {
        let mut data = Vec::new();
        for cert in certs {
            SignatureList::X509(SignatureData::new_x509(owner, Cow::Borrowed(*cert)))
                .extend_as_spec_signature_list(&mut data);
        }
        data
    }

    fn signature_set(data: &[u8]) -> SignatureSet {
        collect_signature_set(data).unwrap()
    }

    #[test]
    fn empty_secure_boot_variable_contains_no_signatures() {
        assert!(signature_set(&[]).is_empty());
    }

    #[test]
    fn base_secure_boot_template_variable_counts_missing_entries() {
        let base_data = x509_variable(TEST_OWNER, &[b"cert1", b"cert2"]);
        let loaded_data = x509_variable(TEST_OWNER, &[b"cert1", b"cert3"]);
        let base = signature_set(&base_data);
        let loaded = signature_set(&loaded_data);
        let missing_entries = base.difference(&loaded).count();

        assert_eq!(base.len(), 2);
        assert_eq!(loaded.len(), 2);
        assert_eq!(missing_entries, 1);
    }

    #[test]
    fn secure_boot_template_comparison_ignores_signature_owner() {
        let baseline = signature_set(&x509_variable(TEST_OWNER, &[b"cert1"]));
        let loaded = signature_set(&x509_variable(Guid::new_random(), &[b"cert1"]));

        assert_eq!(baseline, loaded);
    }

    #[test]
    fn secure_boot_template_variable_skips_auth_header() {
        let data = x509_variable(TEST_OWNER, &[b"cert1"]);
        let mut authenticated_data = EFI_VARIABLE_AUTHENTICATION_2::DUMMY.as_bytes().to_vec();
        authenticated_data.extend_from_slice(&data);

        assert_eq!(signature_set(&authenticated_data), signature_set(&data));
    }

    #[test]
    fn secure_boot_template_variable_rejects_invalid_auth_header_length() {
        let mut data = EFI_VARIABLE_AUTHENTICATION_2::DUMMY.as_bytes().to_vec();
        let length_offset = size_of_val(&EFI_VARIABLE_AUTHENTICATION_2::DUMMY.timestamp);
        data[length_offset..length_offset + size_of::<u32>()]
            .copy_from_slice(&u32::MAX.to_ne_bytes());

        assert_eq!(signature_list_payload(&data), data);
    }

    /// An ephemeral implementation of [`StorageBackend`] backed by an in-memory
    /// buffer. Useful for tests, stateless VM scenarios.
    #[derive(Default)]
    pub struct EphemeralStorageBackend(Option<Vec<u8>>);

    #[async_trait::async_trait]
    impl StorageBackend for EphemeralStorageBackend {
        async fn persist(&mut self, data: Vec<u8>) -> Result<(), StorageBackendError> {
            self.0 = Some(data);
            Ok(())
        }

        async fn restore(&mut self) -> Result<Option<Vec<u8>>, StorageBackendError> {
            Ok(self.0.clone())
        }
    }

    #[async_test]
    async fn test_single_variable() {
        let mut storage = EphemeralStorageBackend::default();
        let mut nvram = HclCompatNvram::new(&mut storage, None);
        impl_agnostic_tests::test_single_variable(&mut nvram).await;
    }

    #[async_test]
    async fn test_multiple_variable() {
        let mut storage = EphemeralStorageBackend::default();
        let mut nvram = HclCompatNvram::new(&mut storage, None);
        impl_agnostic_tests::test_multiple_variable(&mut nvram).await;
    }

    #[async_test]
    async fn test_next() {
        let mut storage = EphemeralStorageBackend::default();
        let mut nvram = HclCompatNvram::new(&mut storage, None);
        impl_agnostic_tests::test_next(&mut nvram).await;
    }

    #[async_test]
    async fn boundary_conditions() {
        let mut storage = EphemeralStorageBackend::default();
        let mut nvram = HclCompatNvram::new(&mut storage, None);

        let vendor = Guid::new_random();
        let attr = 0x1234;
        let data = vec![0x1, 0x2, 0x3, 0x4, 0x5];
        let timestamp = EFI_TIME::default();

        let name_ok = Ucs2LeVec::from_vec_with_nul(
            std::iter::repeat_n([0, b'a'], (EFI_MAX_VARIABLE_NAME_SIZE / 2) - 1)
                .chain(Some([0, 0]))
                .flat_map(|x| x.into_iter())
                .collect(),
        )
        .unwrap();
        let name_too_big = Ucs2LeVec::from_vec_with_nul(
            std::iter::repeat_n([0, b'a'], EFI_MAX_VARIABLE_NAME_SIZE / 2)
                .chain(Some([0, 0]))
                .flat_map(|x| x.into_iter())
                .collect(),
        )
        .unwrap();

        nvram
            .set_variable(&name_ok, vendor, attr, data.clone(), timestamp)
            .await
            .unwrap();

        let res = nvram
            .set_variable(&name_too_big, vendor, attr, data.clone(), timestamp)
            .await;
        assert!(matches!(res, Err(NvramStorageError::VariableNameTooLong)));

        nvram
            .set_variable(
                &name_ok,
                vendor,
                attr,
                vec![0xff; EFI_MAX_VARIABLE_DATA_SIZE],
                timestamp,
            )
            .await
            .unwrap();

        let res = nvram
            .set_variable(
                &name_ok,
                vendor,
                attr,
                vec![0xff; EFI_MAX_VARIABLE_DATA_SIZE + 1],
                timestamp,
            )
            .await;
        assert!(matches!(res, Err(NvramStorageError::VariableDataTooLong)));

        // make sure we can hit the max-memory error
        loop {
            let res = nvram
                .set_variable(
                    &name_ok,
                    Guid::new_random(), // different guids = different vars
                    attr,
                    vec![0xff; EFI_MAX_VARIABLE_DATA_SIZE],
                    timestamp,
                )
                .await;

            match res {
                Ok(()) => {}
                Err(NvramStorageError::OutOfSpace) => break,
                Err(_) => panic!(),
            }
        }
    }

    #[async_test]
    async fn load_reload() {
        let mut storage = EphemeralStorageBackend::default();

        let vendor1 = Guid::new_random();
        let name1 = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "var1").as_bytes()).unwrap();
        let vendor2 = Guid::new_random();
        let name2 = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "var2").as_bytes()).unwrap();
        let vendor3 = Guid::new_random();
        let name3 = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "var3").as_bytes()).unwrap();
        let attr = 0x1234;
        let data = vec![0x1, 0x2, 0x3, 0x4, 0x5];
        let timestamp = EFI_TIME::default();

        let mut nvram = HclCompatNvram::new(&mut storage, None);
        nvram
            .set_variable(name1, vendor1, attr, data.clone(), timestamp)
            .await
            .unwrap();
        nvram
            .set_variable(name2, vendor2, attr, data.clone(), timestamp)
            .await
            .unwrap();
        nvram
            .set_variable(name3, vendor3, attr, data.clone(), timestamp)
            .await
            .unwrap();

        drop(nvram);

        // reload
        let mut nvram = HclCompatNvram::new(&mut storage, None);

        let (result_attr, result_data, result_timestamp) =
            nvram.get_variable(name1, vendor1).await.unwrap().unwrap();
        assert_eq!(result_attr, attr);
        assert_eq!(result_data, data);
        assert_eq!(result_timestamp, timestamp);

        let (result_attr, result_data, result_timestamp) =
            nvram.get_variable(name2, vendor2).await.unwrap().unwrap();
        assert_eq!(result_attr, attr);
        assert_eq!(result_data, data);
        assert_eq!(result_timestamp, timestamp);

        let (result_attr, result_data, result_timestamp) =
            nvram.get_variable(name3, vendor3).await.unwrap().unwrap();
        assert_eq!(result_attr, attr);
        assert_eq!(result_data, data);
        assert_eq!(result_timestamp, timestamp);
    }
}
