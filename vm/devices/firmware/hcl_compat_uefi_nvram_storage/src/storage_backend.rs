// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trait for abstracting the backend used for NVRAM storage

use thiserror::Error;

/// Error when accessing a [`StorageBackend`]
#[derive(Error, Debug)]
#[error("error accessing nvram storage backend")]
pub struct StorageBackendError(#[from] anyhow::Error);

impl StorageBackendError {
    /// Create a new [`StorageBackendError`]
    pub fn new(e: impl Into<anyhow::Error>) -> StorageBackendError {
        Self(e.into())
    }
}

/// Storage backend for accessing the NVRAM
#[async_trait::async_trait]
pub trait StorageBackend: Send + Sync {
    /// Write `data` to a non-volatile storage medium.
    async fn persist(&mut self, data: Vec<u8>) -> Result<(), StorageBackendError>;

    /// Read any previously written `data`. Returns `None` if no data exists.
    async fn restore(&mut self) -> Result<Option<Vec<u8>>, StorageBackendError>;
}

// Boilerplate: forward `StorageBackend` methods for `Box<dyn StorageBackend>`
#[async_trait::async_trait]
impl StorageBackend for Box<dyn StorageBackend> {
    async fn persist(&mut self, data: Vec<u8>) -> Result<(), StorageBackendError> {
        (**self).persist(data).await
    }

    async fn restore(&mut self) -> Result<Option<Vec<u8>>, StorageBackendError> {
        (**self).restore().await
    }
}

// Boilerplate: forward `StorageBackend` methods for `&mut StorageBackend`
#[async_trait::async_trait]
impl<T> StorageBackend for &mut T
where
    T: StorageBackend,
{
    async fn persist(&mut self, data: Vec<u8>) -> Result<(), StorageBackendError> {
        (**self).persist(data).await
    }

    async fn restore(&mut self) -> Result<Option<Vec<u8>>, StorageBackendError> {
        (**self).restore().await
    }
}
