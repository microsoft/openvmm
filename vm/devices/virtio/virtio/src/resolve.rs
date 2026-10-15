// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver definitions for virtio devices.

use crate::DynVirtioDevice;
use crate::VirtioDevice;
use vm_resource::CanResolveTo;
use vm_resource::kind::VirtioDeviceHandle;
use vmcore::vm_task::VmTaskDriverSource;

impl CanResolveTo<ResolvedVirtioDevice> for VirtioDeviceHandle {
    type Input<'a> = VirtioResolveInput<'a>;
}

/// A resolved virtio device.
pub struct ResolvedVirtioDevice(pub Box<dyn DynVirtioDevice>);

impl<T: 'static + VirtioDevice> From<T> for ResolvedVirtioDevice {
    fn from(value: T) -> Self {
        Self(Box::new(value))
    }
}

/// Resolver input for [`VirtioDeviceHandle`].
pub struct VirtioResolveInput<'a> {
    /// The VM driver source.
    pub driver_source: &'a VmTaskDriverSource,
    /// Memory used by the transport. External vhost-user servers supply it at activation.
    /// `None` defers admission; devices must check the actual activation memory.
    pub guest_memory: Option<&'a guestmem::GuestMemory>,
}

impl VirtioResolveInput<'_> {
    /// Whether payload I/O requires owned staging.
    pub fn bounce_io(&self, force: bool) -> bool {
        force
            || self
                .guest_memory
                .is_some_and(|memory| memory.io_policy() == guestmem::GuestMemoryIoPolicy::CopyOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use vmcore::vm_task::SingleDriverBackend;

    #[async_test]
    async fn actual_memory_decides_staging_without_force(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let direct = guestmem::GuestMemory::allocate(4096);
        let denied = direct.copy_only_view();
        assert!(!denied.supports_kernel_lock());
        for (memory, expected) in [(Some(&direct), false), (Some(&denied), true), (None, false)] {
            let input = VirtioResolveInput {
                driver_source: &driver_source,
                guest_memory: memory,
            };
            assert_eq!(input.bounce_io(false), expected);
            assert!(input.bounce_io(true));
        }
    }
}
