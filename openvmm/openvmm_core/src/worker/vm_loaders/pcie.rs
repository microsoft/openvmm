// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Architecture-neutral PCIe device tree encoding.

use vm_topology::pcie::PcieHostBridge;

/// Encode identity MMIO translations with three PCI address cells, two CPU
/// address cells, and two size cells per window.
pub(super) fn identity_ranges(bridge: &PcieHostBridge) -> Vec<u32> {
    let mut ranges = Vec::with_capacity(14);
    for (space, window) in [
        (0x02000000, bridge.low_mmio),
        (0x03000000, bridge.high_mmio),
    ] {
        if !window.is_empty() {
            let start = window.start();
            let len = window.len();
            ranges.extend_from_slice(&[
                space,
                (start >> 32) as u32,
                start as u32,
                (start >> 32) as u32,
                start as u32,
                (len >> 32) as u32,
                len as u32,
            ]);
        }
    }
    ranges
}
