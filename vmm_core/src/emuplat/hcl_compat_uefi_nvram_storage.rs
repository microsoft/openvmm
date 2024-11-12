// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implement [`StorageBackend`] using [`NonVolatileStore`]
//!
//! These two traits are similar but distinct in order to avoid having things
//! like vmgstool taking a dep on NonVolatileStorage directly, which would
//! significantly bloat their dependency tree.

use hcl_compat_uefi_nvram_storage::storage_backend::StorageBackend;
use hcl_compat_uefi_nvram_storage::storage_backend::StorageBackendError;
use vmcore::non_volatile_store::NonVolatileStore;

/// Struct for adapting an implementor of [`NonVolatileStore`] for use with
/// a consumer of [`StorageBackend`]
pub struct VmgsStorageBackendAdapter(pub Box<dyn NonVolatileStore>);

#[async_trait::async_trait]
impl StorageBackend for VmgsStorageBackendAdapter {
    async fn persist(&mut self, data: Vec<u8>) -> Result<(), StorageBackendError> {
        self.0.persist(data).await.map_err(StorageBackendError::new)
    }

    async fn restore(&mut self) -> Result<Option<Vec<u8>>, StorageBackendError> {
        self.0.restore().await.map_err(StorageBackendError::new)
    }
}
