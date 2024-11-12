// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Extension trait to simplify probing PCI [`ChipsetDevice`] devices.

use crate::spec::hwid::ClassCode;
use crate::spec::hwid::HardwareIds;
use crate::spec::hwid::ProgrammingInterface;
use crate::spec::hwid::Subclass;
use chipset_device::pci::PciConfigSpace;
use chipset_device::ChipsetDevice;

/// An extension trait to simplify probing PCI [`ChipsetDevice`] devices.
pub trait PciChipsetDeviceExt: ChipsetDevice + PciConfigSpace {
    /// Probe the PCI device's BAR registers to retrieve the BAR masks.
    fn probe_bar_masks(&mut self) -> [u32; 6];

    /// Probe the PCI device's configuration space registers to obtain the
    /// device's hardware ID values.
    fn probe_hardware_ids(&mut self) -> HardwareIds;
}

impl<T: ?Sized> PciChipsetDeviceExt for T
where
    T: ChipsetDevice + PciConfigSpace,
{
    fn probe_bar_masks(&mut self) -> [u32; 6] {
        let mut masks = [0; 6];
        for (i, addr) in (0x10..=0x24).step_by(4).enumerate() {
            let mut buf = 0;
            let old = self
                .pci_cfg_read(addr, &mut buf)
                .now_or_never()
                .map(|_| buf)
                .unwrap_or(0);
            self.pci_cfg_write(addr, !0).unwrap();
            masks[i] = self
                .pci_cfg_read(addr, &mut buf)
                .now_or_never()
                .map(|_| buf)
                .unwrap_or(0);
            self.pci_cfg_write(addr, old).unwrap();
        }
        masks
    }

    fn probe_hardware_ids(&mut self) -> HardwareIds {
        let mut p0 = 0;
        let mut p8 = 0;
        let mut p2c = 0;
        p0 = self
            .pci_cfg_read(0, &mut p0)
            .now_or_never()
            .map(|_| p0)
            .unwrap_or(0);
        p8 = self
            .pci_cfg_read(8, &mut p8)
            .now_or_never()
            .map(|_| p8)
            .unwrap_or(0);
        p2c = self
            .pci_cfg_read(0x2c, &mut p2c)
            .now_or_never()
            .map(|_| p2c)
            .unwrap_or(0);
        HardwareIds {
            vendor_id: p0 as u16,
            device_id: (p0 >> 16) as u16,
            revision_id: p8 as u8,
            prog_if: ProgrammingInterface((p8 >> 8) as u8),
            sub_class: Subclass((p8 >> 16) as u8),
            base_class: ClassCode((p8 >> 24) as u8),
            type0_sub_vendor_id: p2c as u16,
            type0_sub_system_id: (p2c >> 16) as u16,
        }
    }
}
