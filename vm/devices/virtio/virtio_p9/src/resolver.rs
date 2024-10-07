// Copyright (C) Microsoft Corporation. All rights reserved.

//! Defines the resource resolver for virtio-9p devices.

use crate::VirtioPlan9Device;
use plan9::Plan9FileSystem;
use virtio::resolve::VirtioResolveInput;
use virtio::LegacyWrapper;
use virtio::VirtioDevice;
use virtio_resources::p9::VirtioPlan9Handle;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::ResolveResource;

/// Resolver for virtio-9p devices.
pub struct VirtioPlan9Resolver;

declare_static_resolver! {
    VirtioPlan9Resolver,
    (VirtioDeviceHandle, VirtioPlan9Handle),
}

impl ResolveResource<VirtioDeviceHandle, VirtioPlan9Handle> for VirtioPlan9Resolver {
    type Output = Box<dyn VirtioDevice>;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        resource: VirtioPlan9Handle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let device = LegacyWrapper::new(
            input.driver_source,
            VirtioPlan9Device::new(
                &resource.tag,
                Plan9FileSystem::new(&resource.root_path, resource.debug)?,
                input.guest_memory.clone(),
            ),
            input.guest_memory,
        );
        Ok(Box::new(device))
    }
}