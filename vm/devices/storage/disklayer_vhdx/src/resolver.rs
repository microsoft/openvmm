// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for VHDX disk layers.

use crate::VhdxLayer;
use crate::io::MetadataFile;
use async_trait::async_trait;
use disk_backend_resources::layer::VhdxDiskLayerHandle;
use disk_file::host_file::HostFile;
use disk_layered::resolve::ResolveDiskLayerParameters;
use disk_layered::resolve::ResolvedDiskLayer;
use thiserror::Error;
use vhdx::VhdxFile;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskLayerHandleKind;

/// Resolver for [`VhdxDiskLayerHandle`].
pub struct VhdxDiskLayerResolver;

declare_static_async_resolver!(
    VhdxDiskLayerResolver,
    (DiskLayerHandleKind, VhdxDiskLayerHandle)
);

/// Errors from resolving a VHDX disk layer.
#[derive(Debug, Error)]
pub enum ResolveVhdxError {
    /// Failed to initialize host-file I/O.
    #[error("failed to initialize host-file I/O")]
    HostFile(#[source] std::io::Error),
    /// Failed to open the VHDX file.
    #[error("failed to open vhdx")]
    Open(#[source] vhdx::OpenError),
}

#[async_trait]
impl AsyncResolveResource<DiskLayerHandleKind, VhdxDiskLayerHandle> for VhdxDiskLayerResolver {
    type Output = ResolvedDiskLayer;
    type Error = ResolveVhdxError;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: VhdxDiskLayerHandle,
        input: ResolveDiskLayerParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let read_only = resource.read_only || input.read_only;
        let payload_file = HostFile::new(resource.file, input.driver_source.simple())
            .map_err(ResolveVhdxError::HostFile)?;
        let metadata_file = MetadataFile::from_owned(payload_file.owned_file());
        let vhdx = if read_only {
            VhdxFile::open(metadata_file)
                .read_only()
                .await
                .map_err(ResolveVhdxError::Open)?
        } else {
            let driver = input.driver_source.simple();
            VhdxFile::open(metadata_file)
                .writable(&driver)
                .await
                .map_err(ResolveVhdxError::Open)?
        };
        Ok(ResolvedDiskLayer::new(VhdxLayer::new(
            vhdx,
            payload_file,
            read_only,
        )))
    }
}
