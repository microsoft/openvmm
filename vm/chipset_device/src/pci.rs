// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI configuration space access

use crate::ChipsetDevice;
use crate::io::IoResult;

/// Implemented by devices which have a PCI config space.
pub trait PciConfigSpace: ChipsetDevice {
    /// Dispatch a PCI config space read to the device with the given address.
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult;
    /// Dispatch a PCI config space write to the device with the given address.
    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult;

    /// Handle a PCI configuration space read with full routing context.
    ///
    /// This method receives configuration space accesses with the target bus
    /// and function number. The interpretation of `function` depends on the
    /// bus topology: on a legacy PCI bus it carries packed device/function
    /// bits (0..=255), while downstream of a PCIe port the device number is
    /// always zero so all 8 bits represent functions within a single
    /// endpoint.
    ///
    /// A device can distinguish Type 0 (local) from Type 1 (forwarded)
    /// configuration cycles by comparing `target_bus` and `secondary_bus`:
    /// when they are equal the access targets this device directly (Type 0),
    /// otherwise it should be routed downstream (Type 1). An SR-IOV
    /// capable device can use `secondary_bus` together with `target_bus` and
    /// `function` to compute the VF number.
    ///
    /// The default implementation dispatches function 0 to
    /// [`pci_cfg_read`](Self::pci_cfg_read) and returns all-1s for other
    /// functions (the standard "no device present" response). Routing
    /// components (switches, bridges) and multi-function devices should
    /// override this method.
    ///
    /// # Parameters
    /// - `secondary_bus`: The secondary bus number of the downstream port
    ///   that forwarded this access
    /// - `target_bus`: The bus number targeted by the configuration access
    /// - `function`: Device/function identifier — packed device/function on
    ///   a legacy bus, or flat function number on PCIe
    /// - `offset`: Configuration space offset
    /// - `value`: Pointer to receive the read value
    fn pci_cfg_read_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        value: &mut u32,
    ) -> IoResult {
        if secondary_bus == target_bus && function == 0 {
            self.pci_cfg_read(offset, value)
        } else {
            *value = !0;
            IoResult::Ok
        }
    }

    /// Handle a PCI configuration space write with full routing context.
    ///
    /// This method receives configuration space accesses with the target bus
    /// and function number. The interpretation of `function` depends on the
    /// bus topology: on a legacy PCI bus it carries packed device/function
    /// bits (0..=255), while downstream of a PCIe port the device number is
    /// always zero so all 8 bits represent functions within a single
    /// endpoint.
    ///
    /// A device can distinguish Type 0 (local) from Type 1 (forwarded)
    /// configuration cycles by comparing `target_bus` and `secondary_bus`:
    /// when they are equal the access targets this device directly (Type 0),
    /// otherwise it should be routed downstream (Type 1). An SR-IOV
    /// capable device can use `secondary_bus` together with `target_bus` and
    /// `function` to compute the VF number.
    ///
    /// The default implementation dispatches function 0 to
    /// [`pci_cfg_write`](Self::pci_cfg_write) and silently drops writes to
    /// other functions. Routing components (switches, bridges) and
    /// multi-function devices should override this method.
    ///
    /// # Parameters
    /// - `secondary_bus`: The secondary bus number of the downstream port
    ///   that forwarded this access
    /// - `target_bus`: The bus number targeted by the configuration access
    /// - `function`: Device/function identifier — packed device/function on
    ///   a legacy bus, or flat function number on PCIe
    /// - `offset`: Configuration space offset
    /// - `value`: Value to write
    fn pci_cfg_write_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        value: u32,
    ) -> IoResult {
        if secondary_bus == target_bus && function == 0 {
            self.pci_cfg_write(offset, value)
        } else {
            IoResult::Ok
        }
    }

    /// Check if the device has a suggested (bus, device, function) it expects
    /// to be located at.
    ///
    /// The term "suggested" is important here, as it's important to note that
    /// one of the major selling points of PCI was that PCI devices _shouldn't_
    /// need to care about about what PCI address they are initialized at. i.e:
    /// on a physical machine, it shouldn't matter that your fancy GTX 4090 is
    /// plugged into the first vs. second PCI slot.
    ///
    /// ..that said, there are some instances where it makes sense for an
    /// emulated device to declare its suggested PCI address:
    ///
    /// 1. Devices that emulate bespoke PCI devices part of a particular
    ///    system's chipset.
    ///   - e.g: the PIIX4 chipset includes several bespoke PCI devices that are
    ///     required to have specific PCI addresses. While it _would_ be
    ///     possible to relocate them to a different address, it may break OSes
    ///     that assume they exist at those spec-declared addresses.
    /// 2. Multi-function PCI devices
    ///   - In an unfortunate case of inverted responsibilities, there is a
    ///     single bit in the PCI configuration space's `Header` register that
    ///     denotes if a particular PCI card includes multiple functions.
    ///   - Since multi-function devices are pretty rare, `ChipsetDevice` opted
    ///     to model each function as its own device, which in turn implies that
    ///     in order to correctly init a multi-function PCI card, the
    ///     `ChipsetDevice` with function 0 _must_ report if there are other
    ///     functions at the same bus and device.
    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        None
    }
}
