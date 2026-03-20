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
}
