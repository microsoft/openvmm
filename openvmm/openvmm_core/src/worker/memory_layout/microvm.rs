// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM memory layout tests: the fixed microVM chipset layout reserves the
//! 3-GiB to 4-GiB range for MMIO and resumes RAM above 4 GiB.

#[cfg(test)]
mod tests {
    use super::super::*;
    use test_with_tracing::test;

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
}
