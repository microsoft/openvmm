// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI Express topology types.

use memory_range::MemoryRange;

/// A description of a PCI Express Root Complex, as visible to the CPU.
pub struct PcieHostBridge {
    /// A unique integer index of this host bridge in the VM.
    pub index: u32,
    /// PCIe segment number.
    pub segment: u16,
    /// Lowest valid bus number.
    pub start_bus: u8,
    /// Highest valid bus number.
    pub end_bus: u8,
    /// Memory range used for configuration space access.
    pub ecam_range: MemoryRange,
    /// Memory range used for low MMIO.
    pub low_mmio: MemoryRange,
    /// Memory range used for high MMIO.
    pub high_mmio: MemoryRange,
}
