// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Calculate DMA hint value if not provided by host.

use crate::cmdline::Vtl2GpaPoolConfig;
use crate::cmdline::Vtl2GpaPoolLookupTable;
use igvm_defs::PAGE_SIZE_4K;

/// Lookup table for VTL2 DMA hint calculation. This table is used to retrofit
/// dedicated DMA memory for existing configurations known at the time of writing.
/// Dedicated DMA memory is required for devices that survive OpenHCL servicing
/// operations (for example, NVMe and MANA devices). Those devices express
/// their need as the "persistent memory" requirement when they create a DMA client.
/// Since the amount of dedicated DMA memory cannot be changed at runtime, the amount
/// of memory set aside must account for the maximum expected usage.
///
/// This table derives the maximum expected usage based on:
/// 1. The number of expected MANA and NVMe devices and,
/// 2. The amount of DMA each device needs.
///
/// To illustrate the second point, an NVMe device with 64 queue pairs will need
/// the following memory (see `nvme_driver::QueuePair::new` for details):
/// - Submission queue: 4 pages,
/// - Completion queue: 1 page,
/// - Extra memory per queue: 64 pages
///
/// If there are 32 VPs, we expect one queue pair per VP, leading to:
/// - Total per NVMe device: 32 * (4 + 1 + 64) = 32 * 69 = 2208 pages = 8.59 MiB
///
/// What is not easily derivable from this source base is the max number of devices
/// expected in any given VTL2 configuration. We derive that manually from external
/// data sources.
///
/// The inputs are the number of VTL0 VPs (vp_count) and the amount of memory
/// assigned to VTL2 (vtl2_memory_mb). The output is the recommended DMA hint
/// value (dma_hint_mb).
///
/// The table is sorted by VP count, then by assigned memory.
/// (vp_count, vtl2_memory_mb, dma_hint_mb)
const LOOKUP_TABLE_RELEASE: &[(u16, u16, u16); 39] = &[
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
    (16, 70, 2), // Default manifest is 70MiB. Allocate minimal space for a few NVMe queues.
    (16, 234, 12),
    (16, 256, 20), // There is another 16vp/256MB configuration that only requires 18 MB of DMA memory, pick the larger.
    (16, 268, 38),
    (16, 282, 54),
    (24, 420, 66),
    (32, 404, 22),
    (32, 516, 36),
    (32, 538, 74), // There is another 32vp/538MB configuration that only requires 52 MB of DMA memory, pick the larger.
    (48, 558, 32),
    (48, 718, 52),
    (48, 730, 52),
    (48, 746, 78),
    (64, 712, 42),
    (64, 924, 68),
    (64, 938, 68),
    (96, 1030, 64),
    (96, 1042, 114), // There is another 96vp/1042MB configuration that only requires 64 MB of DMA memory, pick the larger.
    (96, 1058, 114), // There is another 96vp/1058MB configuration that only requires 106 MB of DMA memory, pick the larger.
    (96, 1340, 102),
    (96, 1358, 104),
    (96, 1382, 120),
    (112, 1566, 288),
    (128, 1342, 84),
    (128, 1360, 84),
    (896, 12912, 516), // Needs to be validated as the vNIC number is unknown. (TODO, as part of network device keepalive support).
];

/// DEV/TEST ONLY variant of the lookup table above. Since the IGVM manifest specifies additional
/// VTL2 memory for dev (well above what is required for release configs), allow the heuristics
/// to still kick in.
///
/// These are sized for ~ 3 NVMe devices worth of DMA memory.
/// 69 pages per NVMe per VP * 3 NVMe devices = 207 pages per VP.
const LOOKUP_TABLE_DEBUG: &[(u16, u16, u16); 6] = &[
    (4, 496, 4),
    (16, 512, 16), // 16 VP, 512 MB VTL2 memory is a "heavy" Hyper-V Petri VM.
    (32, 1024, 32),
    (32, 1536, 128), // 32 VP "very heavy", with much extra memory above what is required for dev, allocate lots of memory for DMA.
    (64, 1024, 64),
    (128, 1024, 128),
];

const ONE_MB: u64 = 1024 * 1024;

/// Maximum allowed memory size for DMA hint calculation (1 TiB).
const MAX_DMA_HINT_MEM_SIZE: u64 = 0xFFFFFFFF00000;
/// Number of 4K pages in 2MiB.
const PAGES_PER_2MB: u64 = 2 * ONE_MB / PAGE_SIZE_4K;
// To avoid using floats, scale ratios to 1:1000.
const RATIO: u32 = 1_000;

/// Round up to next 2MiB.
fn round_up_to_2mb(pages_4k: u64) -> u64 {
    (pages_4k + (PAGES_PER_2MB - 1)) & !(PAGES_PER_2MB - 1)
}

/// Returns calculated DMA hint value, in 4k pages.
pub fn vtl2_calculate_dma_hint(
    vtl2_gpa_pool_lookup_table: Vtl2GpaPoolLookupTable,
    vp_count: usize,
    mem_size: u64,
) -> u64 {
    let mut dma_hint_4k = 0;
    // Sanity check for the calculated memory size.
    if mem_size > 0 && mem_size < MAX_DMA_HINT_MEM_SIZE {
        let mem_size_mb = (mem_size / ONE_MB) as u32;
        #[cfg(test)]
        tracing::info!(?vp_count, ?mem_size_mb, "Calculating VTL2 DMA hint",);

        let mut min_vtl2_memory_mb = u16::MAX; // minimum VTL2 memory seen for a given VP count.
        let mut max_vtl2_memory_mb = 0; // maximum VTL2 memory seen for a given VP count.

        let mut min_ratio_1000th = 100 * RATIO;
        let mut max_ratio_1000th = RATIO;

        let mut min_vp_count: u16 = 1; // Biggest VP count entry in the table that is less than vp_count.
        let mut max_vp_count = vp_count as u16; // Smallest VP count entry in the table that is greater than vp_count, or vp_count itself.

        let lookup_table = match vtl2_gpa_pool_lookup_table {
            Vtl2GpaPoolLookupTable::Release => LOOKUP_TABLE_RELEASE.iter(),
            Vtl2GpaPoolLookupTable::Debug => LOOKUP_TABLE_DEBUG.iter(),
        };

        // Take a first loop over the table. Ideally the table contains an exact match
        // for the given VP count and memory size. If not, gather data for extrapolation.
        for (vp_lookup, vtl2_memory_mb, dma_hint_mb) in lookup_table.clone() {
            match (*vp_lookup).cmp(&(vp_count as u16)) {
                core::cmp::Ordering::Less => {
                    // Current entry has fewer VPs than requested.
                    min_vp_count = min_vp_count.max(*vp_lookup);
                }
                core::cmp::Ordering::Equal => {
                    if *vtl2_memory_mb == mem_size_mb as u16 {
                        // Found exact match.
                        dma_hint_4k = *dma_hint_mb as u64 * ONE_MB / PAGE_SIZE_4K;
                        max_vtl2_memory_mb = *vtl2_memory_mb;

                        break;
                    } else {
                        // Prepare for possible extrapolation.
                        min_vtl2_memory_mb = min_vtl2_memory_mb.min(*vtl2_memory_mb);
                        max_vtl2_memory_mb = max_vtl2_memory_mb.max(*vtl2_memory_mb);
                        min_ratio_1000th = min_ratio_1000th
                            .min(*vtl2_memory_mb as u32 * RATIO / *dma_hint_mb as u32);
                        max_ratio_1000th = max_ratio_1000th
                            .max(*vtl2_memory_mb as u32 * RATIO / *dma_hint_mb as u32);
                    }
                }
                core::cmp::Ordering::Greater => {
                    // Current entry has more VPs than requested.
                    // Update the max VP count based on the table. This will be the vp_count unless
                    // the biggest vp count in the table is smaller than the supplied vp_count.

                    max_vp_count = max_vp_count.min(*vp_lookup);
                }
            }
        }

        // Take a second pass over the table if no exact match was found
        // (i.e. unexpected VP count).
        //
        // If there was an exact match for VP count but not for memory size in the table,
        // then we know the min and max ratios for that VP count. But, we also didn't know
        // at that time if there was not going to be an exact match, now go look up the ratios
        // for the nearest VP counts as well.
        if max_vtl2_memory_mb == 0 {
            #[cfg(test)]
            tracing::warn!(
                ?min_vp_count,
                ?max_vp_count,
                ?min_vtl2_memory_mb,
                ?max_vtl2_memory_mb,
                ?min_ratio_1000th,
                ?max_ratio_1000th,
                "Exact match not found, extrapolating DMA hint",
            );
            lookup_table
                .filter(|(vp_lookup, _, _)| {
                    *vp_lookup == min_vp_count || *vp_lookup == max_vp_count
                })
                .for_each(|(_vp_count, vtl2_memory_mb, dma_hint_mb)| {
                    min_vtl2_memory_mb = min_vtl2_memory_mb.min(*vtl2_memory_mb);
                    max_vtl2_memory_mb = max_vtl2_memory_mb.max(*vtl2_memory_mb);
                    min_ratio_1000th =
                        min_ratio_1000th.min(*vtl2_memory_mb as u32 * RATIO / *dma_hint_mb as u32);
                    max_ratio_1000th =
                        max_ratio_1000th.max(*vtl2_memory_mb as u32 * RATIO / *dma_hint_mb as u32);
                });
        }

        if dma_hint_4k == 0 {
            // Didn't find an exact match for vp_count, try to extrapolate.
            dma_hint_4k = (mem_size_mb as u64 * RATIO as u64 * (ONE_MB / PAGE_SIZE_4K))
                / ((min_ratio_1000th + max_ratio_1000th) as u64 / 2u64);

            // And then round up to 2MiB.
            dma_hint_4k = round_up_to_2mb(dma_hint_4k);

            #[cfg(test)]
            tracing::debug!(
                ?min_vp_count,
                ?max_vp_count,
                ?min_vtl2_memory_mb,
                ?max_vtl2_memory_mb,
                ?min_ratio_1000th,
                ?max_ratio_1000th,
                ?dma_hint_4k,
                "Extrapolated VTL2 DMA hint",
            );

            log::info!(
                "Extrapolated VTL2 DMA hint: {} pages ({} MiB) for {} VPs and {} MiB VTL2 memory",
                dma_hint_4k,
                dma_hint_4k * PAGE_SIZE_4K / ONE_MB,
                vp_count,
                mem_size_mb
            );
        } else {
            log::info!(
                "Found exact VTL2 DMA hint: {} pages ({} MiB) for {} VPs and {} MiB VTL2 memory",
                dma_hint_4k,
                dma_hint_4k * PAGE_SIZE_4K / ONE_MB,
                vp_count,
                mem_size_mb
            );
        }
    }

    dma_hint_4k
}

// Decide if we will reserve memory for a VTL2 private pool. See `Vtl2GpaPoolConfig` for
// details.
pub fn pick_private_pool_size(
    cmdline: Vtl2GpaPoolConfig,
    dt: Option<u64>,
    vp_count: usize,
    mem_size: u64,
) -> Option<u64> {
    match (cmdline, dt) {
        (Vtl2GpaPoolConfig::Off, _) => {
            // Command line explicitly disabled the pool.
            log::info!("vtl2 gpa pool disabled via command line");
            None
        }
        (Vtl2GpaPoolConfig::Pages(cmd_line_pages), _) => {
            // Command line specified explicit size, use it.
            log::info!(
                "vtl2 gpa pool enabled via command line with pages: {}",
                cmd_line_pages
            );
            Some(cmd_line_pages)
        }
        (Vtl2GpaPoolConfig::Heuristics(table), None)
        | (Vtl2GpaPoolConfig::Heuristics(table), Some(0)) => {
            // Nothing more explicit, so use heuristics.
            log::info!("vtl2 gpa pool coming from heuristics table: {:?}", table);
            Some(vtl2_calculate_dma_hint(table, vp_count, mem_size))
        }
        (Vtl2GpaPoolConfig::Heuristics(_), Some(dt_page_count)) => {
            // Command line specified heuristics, and the host specified size via device tree. Use
            // the DT.
            log::info!(
                "vtl2 gpa pool enabled via device tree with pages: {}",
                dt_page_count
            );
            Some(dt_page_count)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use test_with_tracing::test;

    const ONE_MB: u64 = 0x10_0000;

    #[test]
    fn test_vtl2_calculate_dma_hint_release() {
        assert_eq!(
            vtl2_calculate_dma_hint(Vtl2GpaPoolLookupTable::Release, 2, 0x620_0000),
            4 * ONE_MB / PAGE_SIZE_4K
        );
        assert_eq!(
            vtl2_calculate_dma_hint(Vtl2GpaPoolLookupTable::Release, 4, 0x6E0_0000),
            6 * ONE_MB / PAGE_SIZE_4K
        );

        // Test VP count higher than max from LOOKUP_TABLE.
        assert_eq!(
            vtl2_calculate_dma_hint(Vtl2GpaPoolLookupTable::Release, 112, 0x700_0000),
            22 * ONE_MB / PAGE_SIZE_4K
        );

        // Test unusual VP count.
        assert_eq!(
            vtl2_calculate_dma_hint(Vtl2GpaPoolLookupTable::Release, 52, 0x600_0000),
            8 * ONE_MB / PAGE_SIZE_4K
        );
        assert_eq!(
            vtl2_calculate_dma_hint(Vtl2GpaPoolLookupTable::Release, 52, 0x800_0000),
            10 * ONE_MB / PAGE_SIZE_4K
        );
    }

    #[test]
    fn test_vtl2_calculate_dma_hint_debug() {
        assert_eq!(
            vtl2_calculate_dma_hint(Vtl2GpaPoolLookupTable::Debug, 4, 496 * ONE_MB),
            4 * ONE_MB / PAGE_SIZE_4K
        );
        assert_eq!(
            vtl2_calculate_dma_hint(Vtl2GpaPoolLookupTable::Debug, 64, 1024 * ONE_MB),
            64 * ONE_MB / PAGE_SIZE_4K
        );
        assert_eq!(
            vtl2_calculate_dma_hint(Vtl2GpaPoolLookupTable::Debug, 128, 1024 * ONE_MB),
            128 * ONE_MB / PAGE_SIZE_4K
        );
        // Extrapolate beyond max memory size from LOOKUP_TABLE.
        assert_eq!(
            vtl2_calculate_dma_hint(Vtl2GpaPoolLookupTable::Debug, 128, 2048 * ONE_MB),
            256 * ONE_MB / PAGE_SIZE_4K
        );
    }

    #[test]
    fn test_vtl2_calculate_dma_hint_exact_matches() {
        for (mode, table) in [
            (Vtl2GpaPoolLookupTable::Release, LOOKUP_TABLE_RELEASE.iter()),
            (Vtl2GpaPoolLookupTable::Debug, LOOKUP_TABLE_DEBUG.iter()),
        ] {
            for (vp_count, vtl2_memory_mb, dma_hint_mb) in table {
                let calculated_dma_hint_4k = vtl2_calculate_dma_hint(
                    mode,
                    *vp_count as usize,
                    (*vtl2_memory_mb as u64) * ONE_MB,
                );
                let expected_dma_hint_4k = (*dma_hint_mb as u64) * ONE_MB / PAGE_SIZE_4K;
                assert_eq!(
                    calculated_dma_hint_4k, expected_dma_hint_4k,
                    "Failed exact match test for vp_count={}, vtl2_memory_mb={}",
                    vp_count, vtl2_memory_mb
                );
            }
        }
    }

    #[test]
    fn test_right_pages_source() {
        // If these assertions fail, the test cases below may need to be updated.
        assert_ne!(
            vtl2_calculate_dma_hint(Vtl2GpaPoolLookupTable::Release, 16, 256 * ONE_MB),
            1500
        );
        assert_ne!(
            vtl2_calculate_dma_hint(Vtl2GpaPoolLookupTable::Debug, 16, 256 * ONE_MB),
            1500
        );

        for (cmdline, dt, expected) in [
            (Vtl2GpaPoolConfig::Off, Some(1000), None),
            (Vtl2GpaPoolConfig::Pages(2000), Some(1000), Some(2000)),
            (Vtl2GpaPoolConfig::Pages(2000), None, Some(2000)),
            (
                Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Release),
                Some(1500),
                Some(1500), // Device tree overrides heuristics.
            ),
            (
                Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Debug),
                Some(0),
                Some(vtl2_calculate_dma_hint(
                    Vtl2GpaPoolLookupTable::Debug,
                    16,
                    256 * ONE_MB,
                )),
            ),
            (
                Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Debug),
                None,
                Some(vtl2_calculate_dma_hint(
                    Vtl2GpaPoolLookupTable::Debug,
                    16,
                    256 * ONE_MB,
                )),
            ),
            (
                Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Release),
                Some(0),
                Some(vtl2_calculate_dma_hint(
                    Vtl2GpaPoolLookupTable::Release,
                    16,
                    256 * ONE_MB,
                )),
            ),
            (
                Vtl2GpaPoolConfig::Heuristics(Vtl2GpaPoolLookupTable::Release),
                None,
                Some(vtl2_calculate_dma_hint(
                    Vtl2GpaPoolLookupTable::Release,
                    16,
                    256 * ONE_MB,
                )),
            ),
        ] {
            let result = pick_private_pool_size(cmdline, dt, 16, 256 * ONE_MB);
            assert_eq!(
                result, expected,
                "Failed pick_private_pool_size test for cmdline={:?}, dt={:?}",
                cmdline, dt
            );
        }
    }
}
