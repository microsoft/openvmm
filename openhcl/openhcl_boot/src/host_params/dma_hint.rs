// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Calculate DMA hint value if not provided by host.

use super::PartitionInfo;
use igvm_defs::{MemoryMapEntryType, PAGE_SIZE_4K};

/// Lookup table for DMA hint calculation.
/// Using tuples instead of structs to keep it readable.
/// Let's keep the table sorted by VP count, then by assigned memory.
/// Using u16 to keep the memory req short.
/// Max VTL2 memory known today is 24838 MiB.
/// (vp_count, vtl2_memory_mb, dma_hint_mb)
const LOOKUP_TABLE: &[(u16, u16, u16)] = &[
    (2, 96, 2),
    (2, 98, 4),
    (2, 100, 4),
    (2, 104, 4),
    (4, 108, 2),
    (4, 110, 6),
    (4, 112, 6),
    (4, 118, 8),
    (4, 130, 12),
    (8, 140, 4),
    (8, 148, 10),
    (8, 170, 20),
    (8, 176, 20),
    (16, 234, 12),
    (16, 256, 20), // There is another configuration with '18'.
    (16, 268, 38),
    (16, 282, 54),
    (24, 420, 66),
    (32, 404, 22),
    (32, 516, 36),
    (32, 538, 74), // There is another configuration with '52'.
    (48, 558, 32),
    (48, 718, 52),
    (48, 730, 52),
    (48, 746, 78),
    (64, 712, 42),
    (64, 924, 68),
    (64, 938, 68),
    (96, 1030, 64),
    (96, 1042, 114), // Can be '64'.
    (96, 1058, 114), // Can be '106'.
    (96, 1340, 102),
    (96, 1358, 104),
    (96, 1382, 120),
    (112, 1566, 288),
    (128, 1342, 84),
    (128, 1360, 84),
    (896, 12912, 0), // (516) Needs to be validated as the vNIC number is unknown.
];

/// Round up to next 2MiB.
fn round_up_to_2mb(pages_4k: u64) -> u64 {
    (pages_4k + 511) & !(511)
}

/// Returns calculated DMA hint value, in 4k pages.
pub fn vtl2_calculate_dma_hint(vp_count: usize, storage: &PartitionInfo) -> u64 {
    let mut dma_hint_4k = 0;
    let mem_size = storage
        .vtl2_ram
        .iter()
        .filter(|m| m.mem_type == MemoryMapEntryType::VTL2_PROTECTABLE)
        .map(|e| e.range.len())
        .sum::<u64>();
    // Sanity check for the calculated memory size.
    if mem_size > 0 && mem_size < 0xFFFFFFFF00000 {
        let mem_size_mb = (mem_size / 1048576) as u32;

        let mut min_vtl2_memory_mb = 65535;
        let mut max_vtl2_memory_mb = 0;

        // To avoid using floats, scale ratios to 1:1000.
        let mut min_ratio_1000th = 100000;
        let mut max_ratio_1000th = 1000;

        let mut min_vp_count: u16 = 1;
        let mut max_vp_count = vp_count as u16;

        for (vp_lookup, vtl2_memory_mb, dma_hint_mb) in LOOKUP_TABLE {
            match (*vp_lookup).cmp(&(vp_count as u16)) {
                core::cmp::Ordering::Less => {
                    // Find nearest.
                    min_vp_count = min_vp_count.max(*vp_lookup);
                }
                core::cmp::Ordering::Equal => {
                    if *vtl2_memory_mb == mem_size_mb as u16 {
                        // Found exact match.
                        dma_hint_4k = *dma_hint_mb as u64 * 1048576 / PAGE_SIZE_4K;
                        max_vtl2_memory_mb = *vtl2_memory_mb;
                        break;
                    } else {
                        // Prepare for possible extrapolation.
                        min_vtl2_memory_mb = min_vtl2_memory_mb.min(*vtl2_memory_mb);
                        max_vtl2_memory_mb = max_vtl2_memory_mb.max(*vtl2_memory_mb);
                        min_ratio_1000th = min_ratio_1000th
                            .min(*vtl2_memory_mb as u32 * 1000 / *dma_hint_mb as u32);
                        max_ratio_1000th = max_ratio_1000th
                            .max(*vtl2_memory_mb as u32 * 1000 / *dma_hint_mb as u32);
                    }
                }
                core::cmp::Ordering::Greater => {
                    // Find nearest.
                    max_vp_count = max_vp_count.min(*vp_lookup);
                }
            }
        }

        // It is possible there were no matching entries in the lookup table.
        // (i.e. unexpected VP count).
        if max_vtl2_memory_mb == 0 {
            LOOKUP_TABLE
                .iter()
                .filter(|(vp_lookup, _, _)| {
                    *vp_lookup == min_vp_count || *vp_lookup == max_vp_count
                })
                .for_each(|(_, vtl2_memory_mb, dma_hint_mb)| {
                    min_vtl2_memory_mb = min_vtl2_memory_mb.min(*vtl2_memory_mb);
                    max_vtl2_memory_mb = max_vtl2_memory_mb.max(*vtl2_memory_mb);
                    min_ratio_1000th =
                        min_ratio_1000th.min(*vtl2_memory_mb as u32 * 1000 / *dma_hint_mb as u32);
                    max_ratio_1000th =
                        max_ratio_1000th.max(*vtl2_memory_mb as u32 * 1000 / *dma_hint_mb as u32);
                });
        }

        if dma_hint_4k == 0 {
            // Didn't find an exact match for vp_count, try to extrapolate.
            dma_hint_4k = (mem_size_mb as u64 * 1000u64 * (1048576u64 / PAGE_SIZE_4K))
                / ((min_ratio_1000th + max_ratio_1000th) as u64 / 2u64);

            // And then round up to 2MiB.
            dma_hint_4k = round_up_to_2mb(dma_hint_4k);
        }
    }

    dma_hint_4k
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::MemoryRange;
    use crate::host_params::MemoryEntry;
    use test_with_tracing::test;

    #[test]
    fn test_vtl2_calculate_dma_hint() {
        let mut storage = PartitionInfo::new();

        storage.vtl2_ram.clear();
        storage.vtl2_ram.push(MemoryEntry {
            range: MemoryRange::new(0x0..0x6200000),
            mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
            vnode: 0,
        });
        assert_eq!(vtl2_calculate_dma_hint(2, &storage), 1024);

        storage.vtl2_ram.clear();
        storage.vtl2_ram.push(MemoryEntry {
            range: MemoryRange::new(0x0..0x6E00000),
            mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
            vnode: 0,
        });
        assert_eq!(vtl2_calculate_dma_hint(4, &storage), 1536);

        // Test VP count higher than max from LOOKUP_TABLE.
        storage.vtl2_ram.clear();
        storage.vtl2_ram.push(MemoryEntry {
            range: MemoryRange::new(0x0..0x7000000),
            mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
            vnode: 0,
        });
        assert_eq!(vtl2_calculate_dma_hint(112, &storage), 5632);

        // Test unusual VP count.
        storage.vtl2_ram.clear();
        storage.vtl2_ram.push(MemoryEntry {
            range: MemoryRange::new(0x0..0x6000000),
            mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
            vnode: 0,
        });
        assert_eq!(vtl2_calculate_dma_hint(52, &storage), 2048);

        storage.vtl2_ram.clear();
        storage.vtl2_ram.push(MemoryEntry {
            range: MemoryRange::new(0x0..0x8000000),
            mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
            vnode: 0,
        });
        assert_eq!(vtl2_calculate_dma_hint(52, &storage), 2560);
    }
}
