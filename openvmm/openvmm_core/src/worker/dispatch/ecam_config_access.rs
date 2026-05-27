// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECAM-based [`PciConfigAccess`] implementation for PCI resource assignment.
//!
//! Routes config space reads/writes through the [`Chipset`]'s MMIO dispatch,
//! exercising the same code path the guest uses.

use pci_resource_assignment::AssignmentError;
use pci_resource_assignment::PciConfigAccess;
use vm_topology::pcie::PcieHostBridge;
use vmotherboard::Chipset;

/// Implements [`PciConfigAccess`] by performing MMIO reads/writes through
/// the [`Chipset`]'s MMIO dispatch at the ECAM address range.
struct EcamConfigAccess<'a> {
    chipset: &'a Chipset,
    ecam_base: u64,
    start_bus: u8,
}

impl<'a> EcamConfigAccess<'a> {
    fn new(chipset: &'a Chipset, ecam_base: u64, start_bus: u8) -> Self {
        Self {
            chipset,
            ecam_base,
            start_bus,
        }
    }

    /// Compute the ECAM GPA for a config space access.
    ///
    /// ECAM layout: each function gets a 4 KiB page.
    /// GPA = ecam_base + ((bus - start_bus) << 20) + (device << 15) + (function << 12) + offset
    fn ecam_addr(&self, bus: u8, device: u8, function: u8, offset: u16) -> u64 {
        let bus_offset = (bus - self.start_bus) as u64;
        self.ecam_base
            + (bus_offset << 20)
            + ((device as u64) << 15)
            + ((function as u64) << 12)
            + offset as u64
    }
}

/// Assign PCI resources (bus numbers and BAR addresses) for all root complexes.
///
/// Iterates over each host bridge and runs the resource assignment algorithm
/// via ECAM config space through the chipset's MMIO dispatch.
pub async fn assign_pci_resources_for_root_complexes(
    chipset: &Chipset,
    pcie_host_bridges: &[PcieHostBridge],
) -> Result<(), AssignmentError> {
    for hb in pcie_host_bridges {
        let params = pci_resource_assignment::AssignmentParams {
            start_bus: hb.start_bus,
            end_bus: hb.end_bus,
            low_mmio: (!hb.low_mmio.is_empty()).then(|| pci_resource_assignment::MmioAperture {
                base: hb.low_mmio.start(),
                len: hb.low_mmio.len(),
            }),
            high_mmio: (!hb.high_mmio.is_empty()).then(|| pci_resource_assignment::MmioAperture {
                base: hb.high_mmio.start(),
                len: hb.high_mmio.len(),
            }),
        };
        let mut ecam = EcamConfigAccess::new(chipset, hb.ecam_range.start(), hb.start_bus);
        pci_resource_assignment::assign_pci_resources(&mut ecam, &params).await?;
    }
    Ok(())
}

impl PciConfigAccess for EcamConfigAccess<'_> {
    async fn read_u32(&mut self, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
        let addr = self.ecam_addr(bus, device, function, offset);
        let mut data = [0u8; 4];
        self.chipset.mmio_read(0, addr, &mut data).await;
        u32::from_le_bytes(data)
    }

    async fn write_u32(&mut self, bus: u8, device: u8, function: u8, offset: u16, value: u32) {
        let addr = self.ecam_addr(bus, device, function, offset);
        self.chipset.mmio_write(0, addr, &value.to_le_bytes()).await;
    }
}
