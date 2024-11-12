// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver definitions for virtio devices.

use crate::VirtioDevice;
use guestmem::GuestMemory;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::CanResolveTo;
use vmcore::vm_task::VmTaskDriverSource;

impl CanResolveTo<Box<dyn VirtioDevice>> for VirtioDeviceHandle {
    type Input<'a> = VirtioResolveInput<'a>;
}

/// Resolver input for [`VirtioDeviceHandle`].
pub struct VirtioResolveInput<'a> {
    /// The VM driver source.
    pub driver_source: &'a VmTaskDriverSource,
    /// The guest memory for virtio device DMA.
    pub guest_memory: &'a GuestMemory,
}
