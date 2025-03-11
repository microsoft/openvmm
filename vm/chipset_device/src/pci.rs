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
