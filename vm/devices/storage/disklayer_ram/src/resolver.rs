// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for RAM-backed disk layers.

use super::Error;
use super::RamDiskLayer;
use crate::LazyRamDiskLayer;
use disk_backend_resources::layer::RamDiskLayerHandle;
use disk_layered::resolve::ResolveDiskLayerParameters;
use disk_layered::resolve::ResolvedDiskLayer;
use vm_resource::declare_static_resolver;
use vm_resource::kind::DiskLayerHandleKind;
use vm_resource::ResolveResource;

/// Resolver for a [`RamDiskLayerHandle`].
pub struct RamDiskLayerResolver;

declare_static_resolver!(
    RamDiskLayerResolver,
    (DiskLayerHandleKind, RamDiskLayerHandle)
);

/// Error type for [`RamDiskLayerResolver`].
#[derive(Debug, Error)]
pub enum ResolveRamDiskError {
    /// Failed to create the RAM disk layer.
    #[error("failed to create ram disk layer")]
    Ram(#[source] Error),
}

impl ResolveResource<DiskLayerHandleKind, RamDiskLayerHandle> for RamDiskLayerResolver {
    type Output = ResolvedDiskLayer;
    type Error = ResolveRamDiskError;

    fn resolve(
        &self,
        rsrc: RamDiskLayerHandle,
        _input: ResolveDiskLayerParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(match rsrc.len {
            Some(len) => {
                ResolvedDiskLayer::new(RamDiskLayer::new(len).map_err(ResolveRamDiskError::Ram)?)
            }
            None => ResolvedDiskLayer::new(LazyRamDiskLayer::new()),
        })
    }
}
