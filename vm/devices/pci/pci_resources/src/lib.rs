// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for PCI devices.

#![forbid(unsafe_code)]

use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device_resources::ErasedChipsetDevice;
use chipset_device_resources::ResolvedChipsetDevice;
use guestmem::DoorbellRegistration;
use guestmem::MemoryMapper;
use pci_core::dma::DmaTarget;
#[cfg(target_os = "linux")]
use std::os::fd::BorrowedFd;
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

/// Hypervisor context for direct iommufd attachment.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
pub struct DirectIommuResolveContext<'a> {
    /// Hypervisor VM fd used to create the direct kernel vIOMMU.
    pub vm_fd: BorrowedFd<'a>,
    /// Stable identity of the root-complex/vIOMMU context.
    pub context_id: u32,
}

/// Parameters used when resolving a resource with kind [`PciDeviceHandleKind`].
pub struct ResolvePciDeviceHandleParams<'a> {
    /// DMA and MSI target for the device.
    pub dma_target: &'a DmaTarget,
    /// An object with which to register MMIO regions.
    pub register_mmio: &'a mut (dyn RegisterMmioIntercept + Send),
    /// The VM's task driver source.
    pub driver_source: &'a VmTaskDriverSource,
    /// An object with which to register doorbell regions.
    pub doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
    /// An object with which to register shared memory regions.
    pub shared_mem_mapper: Option<&'a dyn MemoryMapper>,
    /// Hypervisor context for direct iommufd attach.
    #[cfg(target_os = "linux")]
    pub direct_iommu: Option<DirectIommuResolveContext<'a>>,
}
