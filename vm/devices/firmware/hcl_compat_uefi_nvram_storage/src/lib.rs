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

use cvm_tracing::CVM_ALLOWED;
use cvm_tracing::CVM_CONFIDENTIAL;
use guid::Guid;
use std::fmt::Debug;
use storage_backend::StorageBackend;
use ucs2::Ucs2LeSlice;
use uefi_nvram_storage::EFI_TIME;
use uefi_nvram_storage::NextVariable;
use uefi_nvram_storage::NvramStorage;
use uefi_nvram_storage::NvramStorageError;
use uefi_nvram_storage::in_memory;
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
}

/// "Quirks" to take into account when loading/storing nvram blob data.
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub struct HclCompatNvramQuirks {
    /// When loading nvram variables from storage, don't fail the entire load
    /// process when encountering variables that are missing null terminators in
    /// their name. Instead, skip loading any such variables, and continue on
    /// with the load.
    ///
    /// # Context
    ///
    /// Due to a (now fixed) bug in a previous version of Microsoft HCL, it was
    /// possible for non-null-terminated nvram variables to slip-through
    /// validation and get persisted to disk.
    ///
    /// Enabling this quirk will allow "salvaging" the rest of the non-corrupt
    /// nvram variables, which may be preferable over having the VM fail to boot
    /// at all.
    pub skip_corrupt_vars_with_missing_null_term: bool,
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
        }
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
        Ok(())
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
    use ucs2::Ucs2LeVec;
    use uefi_nvram_storage::in_memory::impl_agnostic_tests;
    use wchar::wchz;

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
