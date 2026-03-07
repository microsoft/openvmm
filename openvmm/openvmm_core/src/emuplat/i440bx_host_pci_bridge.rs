// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chipset_legacy::i440bx_host_pci_bridge::AdjustGpaRange;
use chipset_legacy::i440bx_host_pci_bridge::GpaState;
use futures::executor::block_on;
use membacking::RamVisibility;
use membacking::RamVisibilityControl;
use memory_range::MemoryRange;

/// Implementation of [`AdjustGpaRange`] used by the legacy PCI bus for OpenVMM
/// and the legacy PCAT and SVGA BIOSes.
pub struct ManageRamGpaRange {
    memory: RamVisibilityControl,
}

impl ManageRamGpaRange {
    pub fn new(memory: RamVisibilityControl) -> Self {
        Self { memory }
    }
}

impl AdjustGpaRange for ManageRamGpaRange {
    fn adjust_gpa_range(&mut self, range: MemoryRange, state: GpaState) {
        let state = match state {
            GpaState::Writable => RamVisibility::ReadWrite,
            GpaState::WriteProtected => {
                if range == MemoryRange::new(0xc8000..0xcc000) {
                    // HACK: Our SVGABIOS expects to be able to write in this
                    // segment, but our BIOS does not know that.
                    RamVisibility::ReadWrite
                } else {
                    RamVisibility::ReadOnly
                }
            }
            GpaState::WriteOnly | GpaState::Mmio => {
                // Let reads of unused ranges go to RAM to avoid emulation.
                //
                // TODO: just add a low-pri zero region instead.
                if range.overlaps(&MemoryRange::new(0xcc000..0xe0000)) {
                    RamVisibility::ReadOnly
                } else {
                    RamVisibility::Unmapped
                }
            }
        };
        block_on(self.memory.set_ram_visibility(range, state)).unwrap();
    }
}
