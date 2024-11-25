// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the encrypted disk device.

use crate::CryptDisk;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedSimpleDisk;
use disk_crypt_resources::DiskCryptHandle;
use std::sync::Arc;
use thiserror::Error;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;

declare_static_async_resolver! {
    DiskCryptResolver,
    (DiskHandleKind, DiskCryptHandle),
}

/// The resolver for [`DiskCryptHandle`].
pub struct DiskCryptResolver;

/// An error that occurred while resolving a [`DiskCryptHandle`].
#[derive(Debug, Error)]
pub enum DiskResolveError {
    /// Failed to resolve the inner disk.
    #[error("failed to resolve inner disk")]
    ResolveInner(#[source] ResolveError),
    /// Failed to create the disk.
    #[error("failed to create disk")]
    NewDisk(#[source] crate::NewDiskError),
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, DiskCryptHandle> for DiskCryptResolver {
    type Output = ResolvedSimpleDisk;
    type Error = DiskResolveError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: DiskCryptHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let inner = resolver
            .resolve(
                resource.disk,
                ResolveDiskParameters {
                    read_only: input.read_only,
                    _async_trait_workaround: &(),
                },
            )
            .await
            .map_err(DiskResolveError::ResolveInner)?;

        let disk = CryptDisk::new(resource.cipher, &resource.key, inner.0)
            .map_err(DiskResolveError::NewDisk)?;
        Ok(ResolvedSimpleDisk(Arc::new(disk)))
    }
}
