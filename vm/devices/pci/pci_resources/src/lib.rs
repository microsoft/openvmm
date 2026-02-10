// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for PCI devices.

#![forbid(unsafe_code)]

use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device_resources::ErasedChipsetDevice;
use chipset_device_resources::ResolvedChipsetDevice;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use guestmem::MemoryMapper;
use pci_core::msi::MsiTarget;
use std::sync::Arc;
use vm_resource::CanResolveTo;
use vm_resource::kind::PciDeviceHandleKind;
use vmcore::vm_task::VmTaskDriverSource;

impl CanResolveTo<ResolvedPciDevice> for PciDeviceHandleKind {
    type Input<'a> = ResolvePciDeviceHandleParams<'a>;
}

/// A resolved PCI device.
pub struct ResolvedPciDevice(pub ErasedChipsetDevice);

impl<T: Into<ResolvedChipsetDevice>> From<T> for ResolvedPciDevice {
    fn from(value: T) -> Self {
        Self(value.into().0)
    }
}

/// Parameters used when resolving a resource with kind [`PciDeviceHandleKind`].
pub struct ResolvePciDeviceHandleParams<'a> {
    /// The target for MSI interrupts.
    pub msi_target: &'a MsiTarget,
    /// An object with which to register MMIO regions.
    pub register_mmio: &'a mut (dyn RegisterMmioIntercept + Send),
    /// The VM's task driver source.
    pub driver_source: &'a VmTaskDriverSource,
    /// The VM's guest memory.
    pub guest_memory: &'a GuestMemory,
    /// An object with which to register doorbell regions.
    pub doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
    /// An object with which to register shared memory regions.
    pub shared_mem_mapper: Option<&'a dyn MemoryMapper>,
}
