// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for Qcow2 disk layers.

use crate::Qcow2Layer;
use crate::header::Qcow2Header;
use anyhow::Context;
use async_trait::async_trait;
use blocking::unblock;
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

#[async_trait]
impl AsyncResolveResource<DiskLayerHandleKind, Qcow2DiskLayerHandle> for Qcow2DiskLayerResolver {
    type Output = ResolvedDiskLayer;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        Qcow2DiskLayerHandle { file, read_only }: Qcow2DiskLayerHandle,
        input: ResolveDiskLayerParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let read_only = read_only || input.read_only;
        let layer = unblock(move || {
            let mut file = file;
            file.seek(std::io::SeekFrom::Start(0))
                .context("failed to seek to the start of the qcow2 file")?;
            let header = Qcow2Header::from_file(&mut file)?;
            file.seek(std::io::SeekFrom::Start(0))
                .context("failed to seek to the start of the qcow2 file")?;
            Qcow2Layer::new(file, header, read_only)
        })
        .await?;
        Ok(ResolvedDiskLayer::new(layer))
    }
}
