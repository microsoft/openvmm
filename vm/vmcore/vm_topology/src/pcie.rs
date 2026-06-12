// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI Express topology types.

use crate::cxl::CfmwsWindowRestrictions;
use memory_range::MemoryRange;

/// CXL-specific host bridge metadata.
pub struct PcieHostBridgeCxlInfo {
    /// Memory range reserved for the CHBCR aperture.
    pub chbcr_range: MemoryRange,
    /// Memory range reserved for the HDM decoder.
    pub hdm_range: MemoryRange,
    /// CFMWS HDM window restrictions.
    pub hdm_window_restrictions: CfmwsWindowRestrictions,
}

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
    /// CXL metadata when this host bridge supports CXL.
    pub cxl: Option<PcieHostBridgeCxlInfo>,
    /// NUMA node affinity for this host bridge.
    pub vnode: Option<u32>,
    /// When true, treat non-zero BAR values found during probing as pinned
    /// addresses. Used for P2P DMA with GPA = HPA.
    pub preserve_bars: bool,
}
