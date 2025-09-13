// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of [`StorageBackend`] for VMGS files opened in VmgsTool

use async_trait::async_trait;
use hcl_compat_uefi_nvram_storage::storage_backend::StorageBackend;
use hcl_compat_uefi_nvram_storage::storage_backend::StorageBackendError;
use thiserror::Error;
use vmgs::Vmgs;

/// A [`StorageBackend`] implementation for VMGS files.
pub struct VmgsStorageBackend {
    vmgs: Vmgs,
    file_id: vmgs_format::FileId,
    encrypted: bool,
}

/// Error returned when a VMGS file is requested to be opened in encrypted mode,
/// but the vmgstool was not compiled with encryption support.
#[derive(Debug, Error)]
#[error("vmgstool was not compiled with encryption support")]
pub struct EncryptionNotSupported;

impl VmgsStorageBackend {
    /// Create a new [`StorageBackend`] object backed by a particular VMGS
    /// file-id.
    pub fn new(
        vmgs: Vmgs,
        file_id: vmgs_format::FileId,
        encrypted: bool,
    ) -> Result<Self, EncryptionNotSupported> {
        if encrypted && !cfg!(with_encryption) {
            return Err(EncryptionNotSupported);
        }
        Ok(Self {
            vmgs,
            file_id,
            encrypted,
        })
    }
}

#[async_trait]
impl StorageBackend for VmgsStorageBackend {
    async fn persist(&mut self, data: Vec<u8>) -> Result<(), StorageBackendError> {
        #[cfg(with_encryption)]
        if self.encrypted {
            self.vmgs
                .write_file_encrypted(self.file_id, &data)
                .await
                .map_err(StorageBackendError::new)?;
            return Ok(());
        }

        assert!(!self.encrypted);

        self.vmgs
            .write_file(self.file_id, &data)
            .await
            .map_err(StorageBackendError::new)?;

        Ok(())
    }

    async fn restore(&mut self) -> Result<Option<Vec<u8>>, StorageBackendError> {
        match self.vmgs.read_file(self.file_id).await {
            Ok(buf) => Ok(Some(buf)),
            Err(vmgs::Error::FileInfoNotAllocated) => Ok(None),
            Err(e) => Err(StorageBackendError::new(e)),
        }
    }
}
