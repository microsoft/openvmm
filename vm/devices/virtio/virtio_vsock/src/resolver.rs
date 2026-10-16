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
        let device = if input.bounce_io(resource.bounce_io) {
            device.with_bounce_io()
        } else {
            device
        };
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
        anyhow::ensure!(
            input
                .guest_memory
                .is_none_or(|memory| { memory.supports_kernel_lock() }),
            "vhost-vsock requires kernel-lockable guest memory"
        );
        Ok(crate::vhost::VhostVsockDevice::new(
            input.driver_source,
            resource.vhost,
            resource.guest_cid,
        )?
        .into())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;

    #[async_test]
    async fn kernel_denied_memory_rejected_before_vhost_creation(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let memory = guestmem::GuestMemory::allocate(4096).copy_only_view();
        let error = VirtioVsockVhostResolver
            .resolve(
                virtio_resources::vsock::VirtioVsockVhostHandle {
                    vhost: std::fs::File::open("Cargo.toml").unwrap().into(),
                    guest_cid: 3,
                },
                VirtioResolveInput {
                    driver_source: &driver_source,
                    guest_memory: Some(&memory),
                },
            )
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "vhost-vsock requires kernel-lockable guest memory"
        );
    }
}
