// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for virtio-console devices.

use crate::VirtioConsoleDevice;
use async_trait::async_trait;
use serial_core::resources::ResolveSerialBackendParams;
use virtio::VirtioDeviceAdapter;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::console::VirtioConsoleHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-console devices.
pub struct VirtioConsoleResolver;

declare_static_async_resolver! {
    VirtioConsoleResolver,
    (VirtioDeviceHandle, VirtioConsoleHandle),
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VirtioConsoleHandle> for VirtioConsoleResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VirtioConsoleHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let io = resolver
            .resolve(
                resource.backend,
                ResolveSerialBackendParams {
                    driver: Box::new(input.driver_source.simple()),
                    _async_trait_workaround: &(),
                },
            )
            .await?;

        let device = VirtioConsoleDevice::new(
            input.driver_source,
            input.guest_memory.clone(),
            io.0.into_io(),
        );

        Ok(VirtioDeviceAdapter::new(device).into())
    }
}
