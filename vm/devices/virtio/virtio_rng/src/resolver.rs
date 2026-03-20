// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtio-rng devices.

use crate::VirtioRngDevice;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::rng::VirtioRngHandle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-rng devices.
pub struct VirtioRngResolver;

declare_static_resolver! {
    VirtioRngResolver,
    (VirtioDeviceHandle, VirtioRngHandle),
}

impl ResolveResource<VirtioDeviceHandle, VirtioRngHandle> for VirtioRngResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        _resource: VirtioRngHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let device = VirtioRngDevice::new(input.driver_source);
        Ok(device.into())
    }
}
