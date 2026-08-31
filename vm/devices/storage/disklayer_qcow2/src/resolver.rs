// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for Qcow2 disk layers.

use crate::{Qcow2Layer, header::Qcow2Header};
use async_trait::async_trait;
use disk_backend_resources::layer::Qcow2DiskLayerHandle;
use disk_layered::resolve::ResolveDiskLayerParameters;
use disk_layered::resolve::ResolvedDiskLayer;
use std::io::Seek;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskLayerHandleKind;

/// Resolver for [`Qcow2DiskLayerHandle`].
pub struct Qcow2DiskLayerResolver;

declare_static_async_resolver!(
    Qcow2DiskLayerResolver,
    (DiskLayerHandleKind, Qcow2DiskLayerHandle)
);

/// Read the virtual disk size (in bytes) from the qcow2 header.
///
/// The header's `size` field is a big-endian u64 at offset 24.

#[async_trait]
impl AsyncResolveResource<DiskLayerHandleKind, Qcow2DiskLayerHandle> for Qcow2DiskLayerResolver {
    type Output = ResolvedDiskLayer;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        mut resource: Qcow2DiskLayerHandle,
        input: ResolveDiskLayerParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let read_only = resource.read_only || input.read_only;

        resource.file.seek(std::io::SeekFrom::Start(0))?;
        let header = Qcow2Header::from_file(&mut resource.file)?;
        resource.file.seek(std::io::SeekFrom::Start(0))?;
        Ok(ResolvedDiskLayer::new(Qcow2Layer::new(
            resource.file,
            header,
            read_only,
        )))
    }
}
