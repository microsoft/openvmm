// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for virtio-blk devices.

use crate::VirtioBlkDevice;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::blk::VirtioBlkHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-blk devices.
pub struct VirtioBlkResolver;

declare_static_async_resolver! {
    VirtioBlkResolver,
    (VirtioDeviceHandle, VirtioBlkHandle),
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VirtioBlkHandle> for VirtioBlkResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VirtioBlkHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disk = resolver
            .resolve(
                resource.disk,
                ResolveDiskParameters {
                    read_only: resource.read_only,
                    driver_source: input.driver_source,
                },
            )
            .await?;

        Ok(VirtioBlkDevice::new(input.driver_source, disk.0, resource.read_only).into())
    }
}
