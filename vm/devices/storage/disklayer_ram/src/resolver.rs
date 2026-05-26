// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for RAM-backed disk layers.

use crate::LazyRamDiskLayer;
use disk_backend_resources::layer::RamDiskLayerHandle;
use disk_layered::resolve::ResolveDiskLayerParameters;
use disk_layered::resolve::ResolvedDiskLayer;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::DiskLayerHandleKind;

/// Resolver for a [`RamDiskLayerHandle`].
pub struct RamDiskLayerResolver;

declare_static_resolver!(
    RamDiskLayerResolver,
    (DiskLayerHandleKind, RamDiskLayerHandle)
);

impl ResolveResource<DiskLayerHandleKind, RamDiskLayerHandle> for RamDiskLayerResolver {
    type Output = ResolvedDiskLayer;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        rsrc: RamDiskLayerHandle,
        _input: ResolveDiskLayerParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let mut layer = LazyRamDiskLayer::new();
        if let Some(len) = rsrc.len {
            layer = layer.with_len(len);
        }
        if let Some(sector_size) = rsrc.sector_size {
            layer = layer.with_sector_size(sector_size);
        }
        Ok(ResolvedDiskLayer::new(layer))
    }
}
