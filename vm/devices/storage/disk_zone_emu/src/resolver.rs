// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::ZoneEmulationDisk;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::ZoneEmulationDiskHandle;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;

/// Resolver for [`ZoneEmulationDiskHandle`].
pub struct ZoneEmulationDiskResolver;

declare_static_async_resolver!(
    ZoneEmulationDiskResolver,
    (DiskHandleKind, ZoneEmulationDiskHandle)
);

/// An error resolving a zoned disk emulator.
#[derive(Debug, Error)]
pub enum ResolveZonedDiskError {
    /// The inner disk could not be resolved.
    #[error("failed to resolve inner disk")]
    Resolve(#[source] ResolveError),
    /// The zoned disk parameters are invalid.
    #[error("invalid zoned disk parameters")]
    InvalidParameters(#[source] crate::InvalidZonedConfiguration),
    /// The resolved disk is invalid.
    #[error("invalid disk")]
    InvalidDisk(#[source] disk_backend::InvalidDisk),
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, ZoneEmulationDiskHandle> for ZoneEmulationDiskResolver {
    type Output = ResolvedDisk;
    type Error = ResolveZonedDiskError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: ZoneEmulationDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let inner = resolver
            .resolve(resource.disk, input)
            .await
            .map_err(ResolveZonedDiskError::Resolve)?;
        let disk = ZoneEmulationDisk::new(inner.0, resource.parameters)
            .map_err(ResolveZonedDiskError::InvalidParameters)?;
        ResolvedDisk::new(disk).map_err(ResolveZonedDiskError::InvalidDisk)
    }
}
