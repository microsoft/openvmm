// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for RAM disks.

use super::Error;
use super::RamLayer;
use async_trait::async_trait;
use disk_backend::layered::DiskLayer;
use disk_backend::layered::LayeredDisk;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::RamDiffDiskHandle;
use disk_backend_resources::RamDiskHandle;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;

/// Resolver for a [`RamDiskHandle`] and [`RamDiffDiskHandle`].
pub struct RamDiskResolver;

declare_static_async_resolver!(
    RamDiskResolver,
    (DiskHandleKind, RamDiskHandle),
    (DiskHandleKind, RamDiffDiskHandle)
);

/// Error type for [`RamDiskResolver`].
#[derive(Debug, Error)]
pub enum ResolveRamDiskError {
    /// Failed to create the RAM disk.
    #[error("failed to create ram disk")]
    Ram(#[source] Error),
    /// Failed to resolve the inner disk.
    #[error("failed to resolve inner disk")]
    Resolve(#[source] vm_resource::ResolveError),
    /// Invalid disk.
    #[error("invalid disk")]
    InvalidDisk(#[source] disk_backend::InvalidDisk),
    /// Invalid layer.
    #[error("invalid layer")]
    InvalidLayer(#[source] disk_backend::layered::InvalidLayer),
    /// Invalid layered disk.
    #[error("invalid layered disk")]
    InvalidLayeredDisk(#[source] disk_backend::layered::InvalidLayeredDisk),
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, RamDiskHandle> for RamDiskResolver {
    type Output = ResolvedDisk;
    type Error = ResolveRamDiskError;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        rsrc: RamDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        ResolvedDisk::new(
            LayeredDisk::new(vec![DiskLayer::new(
                RamLayer::new(rsrc.len, input.read_only).map_err(ResolveRamDiskError::Ram)?,
                Default::default(),
            )
            .map_err(ResolveRamDiskError::InvalidLayer)?])
            .map_err(ResolveRamDiskError::InvalidLayeredDisk)?,
        )
        .map_err(ResolveRamDiskError::InvalidDisk)
    }
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, RamDiffDiskHandle> for RamDiskResolver {
    type Output = ResolvedDisk;
    type Error = ResolveRamDiskError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        rsrc: RamDiffDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let lower = resolver
            .resolve(
                rsrc.lower,
                ResolveDiskParameters {
                    read_only: true,
                    _async_trait_workaround: &(),
                },
            )
            .await
            .map_err(ResolveRamDiskError::Resolve)?;

        let upper = RamLayer::new(
            lower.0.sector_count() * lower.0.sector_size() as u64,
            input.read_only,
        )
        .map_err(ResolveRamDiskError::Ram)?;

        ResolvedDisk::new(
            LayeredDisk::new(vec![
                DiskLayer::new(upper, Default::default())
                    .map_err(ResolveRamDiskError::InvalidLayer)?,
                DiskLayer::from_disk(lower.0),
            ])
            .map_err(ResolveRamDiskError::InvalidLayeredDisk)?,
        )
        .map_err(ResolveRamDiskError::InvalidDisk)
    }
}
