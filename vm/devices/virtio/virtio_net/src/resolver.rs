// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtio-net devices.

use crate::Device;
use async_trait::async_trait;
use net_backend::resolve::ResolveEndpointParams;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::net::VirtioNetHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-pmem devices.
pub struct VirtioNetResolver;

declare_static_async_resolver! {
    VirtioNetResolver,
    (VirtioDeviceHandle, VirtioNetHandle),
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VirtioNetHandle> for VirtioNetResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VirtioNetHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let mut builder = Device::builder();
        if let Some(max_queues) = resource.max_queues {
            builder = builder.max_queues(max_queues);
        }

        let endpoint = resolver
            .resolve(
                resource.endpoint,
                ResolveEndpointParams {
                    mac_address: resource.mac_address,
                },
            )
            .await?;

        let device = builder.build(
            input.driver_source,
            input.guest_memory.clone(),
            endpoint.0,
            resource.mac_address,
        );

        Ok(device.into())
    }
}
