// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of [`NonVolatileStore`] for VMGS files.

use crate::VmgsClient;
use crate::VmgsClientError;
use crate::broker::VmgsBrokerError;
use async_trait::async_trait;
use thiserror::Error;
use vmcore::non_volatile_store::NonVolatileStore;
use vmcore::non_volatile_store::NonVolatileStoreError;

/// A [`NonVolatileStore`] implementation for VMGS files.
pub struct VmgsNonVolatileStore {
    vmgs: VmgsClient,
    file_id: vmgs_format::FileId,
    encrypted: bool,
}

/// Error returned when a VMGS file is requested to be opened in encrypted mode,
/// but the VMGS broker was not compiled with encryption support.
#[derive(Debug, Error)]
#[error("the vmgs_broker crate was not compiled with encryption support")]
pub struct EncryptionNotSupported;

impl VmgsNonVolatileStore {
    /// Create a new [`NonVolatileStore`] object backed by a particular VMGS
    /// file-id.
    pub fn new(
        vmgs: VmgsClient,
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
impl NonVolatileStore for VmgsNonVolatileStore {
    async fn persist(&mut self, data: Vec<u8>) -> Result<(), NonVolatileStoreError> {
        #[cfg(with_encryption)]
        if self.encrypted {
            self.vmgs
                .write_file_encrypted(self.file_id, data)
                .await
                .map_err(NonVolatileStoreError::new)?;
            return Ok(());
        }

        assert!(!self.encrypted);

        self.vmgs
            .write_file(self.file_id, data)
            .await
            .map_err(NonVolatileStoreError::new)?;

        Ok(())
    }

    async fn restore(&mut self) -> Result<Option<Vec<u8>>, NonVolatileStoreError> {
        match self.vmgs.read_file(self.file_id).await {
            Ok(buf) => Ok(Some(buf)),
            Err(VmgsClientError::Vmgs(VmgsBrokerError::FileInfoNotAllocated)) => Ok(None),
            Err(e) => Err(NonVolatileStoreError::new(e)),
        }
    }
}
