// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtio-pmem devices.

use crate::Device;
use virtio::resolve::VirtioResolveInput;
use virtio::VirtioDevice;
use virtio_resources::pmem::VirtioPmemHandle;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::ResolveResource;

/// Resolver for virtio-pmem devices.
pub struct VirtioPmemResolver;

declare_static_resolver! {
    VirtioPmemResolver,
    (VirtioDeviceHandle, VirtioPmemHandle),
}

impl ResolveResource<VirtioDeviceHandle, VirtioPmemHandle> for VirtioPmemResolver {
    type Output = Box<dyn VirtioDevice>;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        resource: VirtioPmemHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let file = fs_err::File::open(resource.path)?.into();
        let device = Device::new(input.driver_source, input.guest_memory.clone(), file, false)?;
        Ok(Box::new(device))
    }
}
