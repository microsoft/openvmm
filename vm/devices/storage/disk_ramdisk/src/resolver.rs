// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for RAM disks.

use super::Error;
use super::RamLayer;
use disk_backend_resources::layer::RamDiskLayerHandle;
use disk_layered::resolve::ResolveDiskLayerParameters;
use disk_layered::resolve::ResolvedDiskLayer;
use vm_resource::declare_static_resolver;
use vm_resource::kind::DiskLayerHandleKind;
use vm_resource::ResolveResource;

/// Resolver for a [`RamDiskLayerHandle`].
pub struct RamDiskResolver;

declare_static_resolver!(RamDiskResolver, (DiskLayerHandleKind, RamDiskLayerHandle));

/// Error type for [`RamDiskResolver`].
#[derive(Debug, Error)]
pub enum ResolveRamDiskError {
    /// Failed to create the RAM disk.
    #[error("failed to create ram disk")]
    Ram(#[source] Error),
}

impl ResolveResource<DiskLayerHandleKind, RamDiskLayerHandle> for RamDiskResolver {
    type Output = ResolvedDiskLayer;
    type Error = ResolveRamDiskError;

    fn resolve(
        &self,
        rsrc: RamDiskLayerHandle,
        _input: ResolveDiskLayerParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedDiskLayer::new(
            RamLayer::new(rsrc.len).map_err(ResolveRamDiskError::Ram)?,
        ))
    }
}
