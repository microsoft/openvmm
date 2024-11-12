// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for PCI devices.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device_resources::ErasedChipsetDevice;
use chipset_device_resources::ResolvedChipsetDevice;
use guestmem::GuestMemory;
use pci_core::msi::RegisterMsi;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::CanResolveTo;
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
    pub register_msi: &'a mut dyn RegisterMsi,
    /// An object with which to register MMIO regions.
    pub register_mmio: &'a mut (dyn RegisterMmioIntercept + Send),
    /// The VM's task driver source.
    pub driver_source: &'a VmTaskDriverSource,
    /// The VM's guest memory.
    pub guest_memory: &'a GuestMemory,
}
