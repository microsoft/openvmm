// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtio-pmem devices.

use crate::Device;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::pmem::VirtioPmemHandle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-pmem devices.
pub struct VirtioPmemResolver;

declare_static_resolver! {
    VirtioPmemResolver,
    (VirtioDeviceHandle, VirtioPmemHandle),
}

impl ResolveResource<VirtioDeviceHandle, VirtioPmemHandle> for VirtioPmemResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        resource: VirtioPmemHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let file = fs_err::File::open(resource.path)?.into();
        let device = Device::new(input.driver_source, input.guest_memory.clone(), file, false)?;
        Ok(device.into())
    }
}
