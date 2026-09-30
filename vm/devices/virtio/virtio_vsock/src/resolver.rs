// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtio-vsock devices.

use crate::VirtioVsockDevice;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::vsock::VirtioVsockHandle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-vsock devices.
pub struct VirtioVsockResolver;

declare_static_resolver! {
    VirtioVsockResolver,
    (VirtioDeviceHandle, VirtioVsockHandle),
}

impl ResolveResource<VirtioDeviceHandle, VirtioVsockHandle> for VirtioVsockResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        resource: VirtioVsockHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let device = VirtioVsockDevice::new(
            input.driver_source,
            resource.guest_cid,
            resource.base_path.into(),
            resource.listener,
        )?;
        Ok(device.into())
    }
}

/// Resolver for Linux kernel `vhost_vsock` devices.
#[cfg(target_os = "linux")]
pub struct VirtioVsockVhostResolver;

#[cfg(target_os = "linux")]
declare_static_resolver! {
    VirtioVsockVhostResolver,
    (VirtioDeviceHandle, virtio_resources::vsock::VirtioVsockVhostHandle),
}

#[cfg(target_os = "linux")]
impl ResolveResource<VirtioDeviceHandle, virtio_resources::vsock::VirtioVsockVhostHandle>
    for VirtioVsockVhostResolver
{
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        resource: virtio_resources::vsock::VirtioVsockVhostHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(crate::vhost::VhostVsockDevice::new(
            input.driver_source,
            resource.vhost,
            resource.guest_cid,
        )?
        .into())
    }
}
