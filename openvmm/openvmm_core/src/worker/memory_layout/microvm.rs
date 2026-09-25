// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM memory-capacity layout support.

use memory_range::MemoryRange;

pub(super) fn validate_memory_capacity(
    node_mem_sizes: &[u64],
    memory_capacity: Option<u64>,
) -> anyhow::Result<()> {
    if let Some(memory_capacity) = memory_capacity {
        anyhow::ensure!(
            node_mem_sizes.len() == 1,
            "RAM capacity requires a single NUMA node"
        );
        anyhow::ensure!(
            memory_capacity >= node_mem_sizes[0],
            "RAM capacity {memory_capacity:#x} is below active RAM size {:#x}",
            node_mem_sizes[0]
        );
        anyhow::ensure!(
            memory_capacity.is_multiple_of(super::PAGE_SIZE),
            "RAM capacity {memory_capacity:#x} is not page-aligned"
        );
    }
    Ok(())
}

pub(super) fn layout_ram_size(vnode: usize, active_size: u64, memory_capacity: Option<u64>) -> u64 {
    if vnode == 0 {
        memory_capacity.unwrap_or(active_size)
    } else {
        active_size
    }
}

/// Trims each node's layout RAM to the guest-visible active RAM, excluding the
/// reserved restore-time capacity.
pub(super) fn active_ram_ranges(
    ranges_by_node: Vec<Vec<MemoryRange>>,
    node_mem_sizes: &[u64],
) -> Vec<Vec<MemoryRange>> {
    ranges_by_node
        .into_iter()
        .zip(node_mem_sizes)
        .map(|(ranges, &active_size)| {
            memory_range_prefix(&ranges, active_size).expect("layout RAM must cover active RAM")
        })
        .collect()
}

fn memory_range_prefix(ranges: &[MemoryRange], size: u64) -> anyhow::Result<Vec<MemoryRange>> {
    let mut remaining = size;
    let mut prefix = Vec::new();
    for range in ranges {
        if remaining == 0 {
            break;
        }
        let length = range.len().min(remaining);
        prefix.push(MemoryRange::new(range.start()..range.start() + length));
        remaining -= length;
    }
    anyhow::ensure!(remaining == 0, "RAM ranges do not cover requested prefix");
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use test_with_tracing::test;

    const MB: u64 = 1024 * 1024;
    #[cfg(guest_arch = "x86_64")]
    const DEFAULT_CHIPSET_LOW_MMIO_SIZE: u32 = 128 * 1024 * 1024;
    #[cfg(guest_arch = "aarch64")]
    const DEFAULT_CHIPSET_LOW_MMIO_SIZE: u32 = 512 * 1024 * 1024;
    const DEFAULT_CHIPSET_HIGH_MMIO_SIZE: u64 = 512 * 1024 * 1024;

    const DEFAULT_LAYOUT: vmm_core_defs::LayoutConfig = vmm_core_defs::LayoutConfig {
        chipset_low_mmio_size: DEFAULT_CHIPSET_LOW_MMIO_SIZE,
        chipset_high_mmio_size: DEFAULT_CHIPSET_HIGH_MMIO_SIZE,
        vtl2_chipset_mmio_size: 0,
    };

    fn input(
        node_mem_sizes: &[u64],
        vtl2_layout: Option<Vtl2MemoryLayoutRequest>,
    ) -> MemoryLayoutInput<'_> {
        MemoryLayoutInput {
            node_mem_sizes,
            memory_capacity: None,
            layout: DEFAULT_LAYOUT,
            pcie_root_complexes: &[],
            virtio_mmio_count: 0,
            pcie_ecam_below_4gb: false,
            vtl2_layout,
            ram_start_address: 0,
            vtl2_framebuffer_size: 0,
            physical_address_size: 46,
        }
    }

    fn resolve(input: MemoryLayoutInput<'_>) -> MemoryLayout {
        resolve_memory_layout(input).unwrap().memory_layout
    }

    #[test]
    fn microvm_ram_ends_at_3gb_and_resumes_at_4gb() {
        let mut config = input(&[4 * GB], None);
        config.layout = vmm_core_defs::LayoutConfig {
            chipset_low_mmio_size: GB as u32,
            chipset_high_mmio_size: 0,
            vtl2_chipset_mmio_size: 0,
        };
        let layout = resolve(config);
        assert_eq!(
            layout
                .ram()
                .iter()
                .map(|range| range.range)
                .collect::<Vec<_>>(),
            [
                MemoryRange::new(0..3 * GB),
                MemoryRange::new(4 * GB..5 * GB),
            ]
        );
    }

    #[test]
    fn memory_capacity_reserves_aperture_without_publishing_it_as_ram() {
        let mut config = input(&[512 * MB], None);
        config.memory_capacity = Some(2 * GB);
        config.layout = vmm_core_defs::LayoutConfig {
            chipset_low_mmio_size: GB as u32,
            chipset_high_mmio_size: GB,
            vtl2_chipset_mmio_size: 0,
        };
        let layout = config.layout.clone();

        let resolved = resolve_memory_layout(config).unwrap();

        assert_eq!(
            resolved.memory_layout.ram(),
            &[MemoryRangeWithNode {
                range: MemoryRange::new(0..512 * MB),
                vnode: 0,
            }]
        );
        let reserved_high_mmio_start = resolved.chipset_mmio.high.start();

        let mut expanded = input(&[GB], None);
        expanded.memory_capacity = Some(2 * GB);
        expanded.layout = layout;
        let expanded = resolve_memory_layout(expanded).unwrap();
        assert_eq!(
            expanded.memory_layout.ram(),
            &[MemoryRangeWithNode {
                range: MemoryRange::new(0..GB),
                vnode: 0,
            }]
        );
        assert_eq!(expanded.chipset_mmio.high.start(), reserved_high_mmio_start);
    }
}
