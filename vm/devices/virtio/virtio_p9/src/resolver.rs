// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtio-9p devices.

use crate::VirtioPlan9Device;
use plan9::Plan9FileSystem;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::p9::VirtioPlan9Handle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-9p devices.
pub struct VirtioPlan9Resolver;

declare_static_resolver! {
    VirtioPlan9Resolver,
    (VirtioDeviceHandle, VirtioPlan9Handle),
}

impl ResolveResource<VirtioDeviceHandle, VirtioPlan9Handle> for VirtioPlan9Resolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        resource: VirtioPlan9Handle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let device = VirtioPlan9Device::new(
            input.driver_source,
            &resource.tag,
            Plan9FileSystem::new(&resource.root_path, resource.debug)?,
            input.guest_memory.clone(),
        );
        Ok(device.into())
    }
}
