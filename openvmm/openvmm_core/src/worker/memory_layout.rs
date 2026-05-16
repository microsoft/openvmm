// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest physical memory layout resolution for the VM worker.
//!
//! This module is the point where OpenVMM turns stable VM configuration and
//! already-known platform ranges into the production [`MemoryLayout`]. The
//! resulting guest physical addresses are part of the VM's compatibility surface:
//! hibernated guests and saved VMs remember device and RAM locations, so changes
//! to the request order, placement class, or alignment policy can break resume or
//! restore. Keep layout policy changes deliberate and covered by tests.
//!
//! The resolver keeps today's MMIO inputs fixed while moving RAM and VTL2
//! placement into `vm_topology::layout`. Fixed ranges are registered first so RAM
//! splits around them. VTL2 is registered last as post-MMIO private memory so it
//! does not perturb the VTL0-visible RAM/MMIO layout.

use super::vm_loaders::igvm::Vtl2MemoryLayoutRequest;
use anyhow::Context;
use anyhow::bail;
use memory_range::MemoryRange;
use vm_topology::layout::LayoutBuilder;
use vm_topology::layout::Placement;
use vm_topology::memory::MemoryLayout;
use vm_topology::memory::MemoryRangeWithNode;

const PAGE_SIZE: u64 = 4096;
const TWO_MB: u64 = 2 * 1024 * 1024;
const GB: u64 = 1024 * 1024 * 1024;

pub(super) struct MemoryLayoutInput<'a> {
    /// Total VTL0 RAM size requested by the VM configuration.
    pub mem_size: u64,
    /// Optional per-vNUMA RAM budgets. When present, these must sum to
    /// `mem_size`, and request order is the vnode assignment order.
    pub numa_mem_sizes: Option<&'a [u64]>,
    /// Existing resolved chipset/MMIO ranges. These are fixed for this
    /// transition step; later commits will move individual consumers to typed
    /// dynamic intents.
    pub mmio_gaps: &'a [MemoryRange],
    /// Existing resolved PCI ECAM ranges, treated as fixed occupied space.
    pub pci_ecam_gaps: &'a [MemoryRange],
    /// Existing resolved PCI MMIO ranges, treated as fixed occupied space.
    pub pci_mmio_gaps: &'a [MemoryRange],
    /// Optional IGVM VTL2 private-memory request. This is allocated after all
    /// VTL0-visible RAM and MMIO and is carried separately from ordinary RAM.
    pub vtl2_layout: Option<Vtl2MemoryLayoutRequest>,
    /// Host-supported physical address width used only after allocation. The
    /// allocator computes the smallest layout it can; host fit is validation.
    pub physical_address_size: u8,
}

pub(super) fn resolve_memory_layout(input: MemoryLayoutInput<'_>) -> anyhow::Result<MemoryLayout> {
    let ram_sizes = validate_ram_sizes(input.mem_size, input.numa_mem_sizes)?;

    let mut ram_ranges_by_node = vec![Vec::new(); ram_sizes.len()];
    let mut vtl2_range = MemoryRange::EMPTY;

    let mut builder = LayoutBuilder::new();
    add_fixed_ranges(&mut builder, "mmio", input.mmio_gaps);
    add_fixed_ranges(&mut builder, "pci_ecam", input.pci_ecam_gaps);
    add_fixed_ranges(&mut builder, "pci_mmio", input.pci_mmio_gaps);

    // RAM request order is part of the NUMA compatibility contract: the first
    // request maps to vnode 0, the second to vnode 1, and so on. For GB-sized
    // nodes, use GB alignment so holes do not create sub-GB RAM chunks. For
    // sub-GB nodes, use 2 MB alignment to avoid wasting a full GB of address
    // space per small node.
    for (vnode, (ram_size, ram_ranges)) in ram_sizes
        .iter()
        .copied()
        .zip(&mut ram_ranges_by_node)
        .enumerate()
    {
        let ram_alignment = if ram_size < GB { TWO_MB } else { GB };
        builder.ram(format!("ram[{vnode}]"), ram_ranges, ram_size, ram_alignment);
    }

    // VTL2 MemoryLayout mode is implementation-private memory, not a VTL0 RAM
    // hole. Allocate it only after all VTL0-visible RAM/MMIO so enabling VTL2
    // does not move the VTL0 layout.
    //
    // IGVM relocation min/max constraints are checked later by the IGVM loader
    // against the selected base; using them as a constraint here would be
    // overconstraining and would lead to holes in the VTL0 layout--we just
    // don't support IGVM files with relocation sections that cannot be
    // satisfied by the post-MMIO space.
    if let Some(vtl2_layout) = input.vtl2_layout {
        builder.request(
            "vtl2",
            &mut vtl2_range,
            vtl2_layout.size,
            vtl2_layout.alignment,
            Placement::PostMmio,
        );
    }

    builder
        .allocate()
        .context("allocating memory layout ranges")?;

    let ram = ram_ranges_by_node
        .into_iter()
        .enumerate()
        .flat_map(|(vnode, ranges)| {
            ranges.into_iter().map(move |range| MemoryRangeWithNode {
                range,
                vnode: vnode as u32,
            })
        })
        .collect::<Vec<_>>();

    let vtl2_range = input.vtl2_layout.map(|_| vtl2_range);

    // `MemoryLayout` remains the shared validation and query type for the rest
    // of the worker. Construct it from resolved RAM so no later consumer repeats
    // RAM placement or infers RAM by subtracting from MMIO gaps.
    let memory_layout = MemoryLayout::new_from_resolved_ranges(
        ram,
        input.mmio_gaps.to_vec(),
        input.pci_ecam_gaps.to_vec(),
        input.pci_mmio_gaps.to_vec(),
        vtl2_range,
    )
    .context("validating resolved memory layout")?;

    // Host address-width validation is intentionally after allocation. The
    // layout engine is host-width independent, which keeps the layout a pure
    // function of VM configuration and avoids host differences changing guest
    // physical addresses.
    let address_space_limit = 1u64 << input.physical_address_size;
    if memory_layout.end_of_layout() > address_space_limit {
        bail!(
            "memory layout ends at {:#x}, which exceeds the address width of {} bits",
            memory_layout.end_of_layout(),
            input.physical_address_size
        );
    }

    Ok(memory_layout)
}

fn add_fixed_ranges(builder: &mut LayoutBuilder<'_>, tag_prefix: &str, ranges: &[MemoryRange]) {
    // These are fixed only from the allocator's point of view. Today they are
    // already-resolved config fields; future commits will replace some of them
    // with typed dynamic requests owned by this resolver.
    for (index, range) in ranges.iter().enumerate() {
        builder.fixed(format!("{tag_prefix}[{index}]"), *range);
    }
}

fn validate_ram_sizes(mem_size: u64, numa_mem_sizes: Option<&[u64]>) -> anyhow::Result<Vec<u64>> {
    // Keep validation compatible with `MemoryLayout::new()` / `new_with_numa()`:
    // RAM sizes are page-granular, nonzero, and NUMA budgets must exactly cover
    // the configured total.
    if mem_size == 0 || !mem_size.is_multiple_of(PAGE_SIZE) {
        bail!("invalid memory size {mem_size:#x}");
    }

    let Some(numa_mem_sizes) = numa_mem_sizes else {
        return Ok(vec![mem_size]);
    };

    if numa_mem_sizes.is_empty() {
        bail!("empty NUMA memory sizes");
    }

    for &size in numa_mem_sizes {
        if size == 0 || !size.is_multiple_of(PAGE_SIZE) {
            bail!("invalid NUMA node memory size {size:#x}");
        }
    }

    let total = numa_mem_sizes
        .iter()
        .copied()
        .try_fold(0u64, |acc, size| acc.checked_add(size))
        .context("numa memory sizes overflow")?;
    if total != mem_size {
        bail!("numa_mem_sizes total ({total:#x}) does not match mem_size ({mem_size:#x})");
    }

    Ok(numa_mem_sizes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_topology::memory::AddressType;

    const MB: u64 = 1024 * 1024;

    fn input<'a>(
        mem_size: u64,
        numa_mem_sizes: Option<&'a [u64]>,
        mmio_gaps: &'a [MemoryRange],
        pci_ecam_gaps: &'a [MemoryRange],
        pci_mmio_gaps: &'a [MemoryRange],
        vtl2_layout: Option<Vtl2MemoryLayoutRequest>,
    ) -> MemoryLayoutInput<'a> {
        MemoryLayoutInput {
            mem_size,
            numa_mem_sizes,
            mmio_gaps,
            pci_ecam_gaps,
            pci_mmio_gaps,
            vtl2_layout,
            physical_address_size: 46,
        }
    }

    fn vtl2_layout(size: u64) -> Vtl2MemoryLayoutRequest {
        Vtl2MemoryLayoutRequest {
            size,
            alignment: PAGE_SIZE,
        }
    }

    #[test]
    fn non_numa_matches_memory_layout_new() {
        let mmio = [
            MemoryRange::new(2 * GB..3 * GB),
            MemoryRange::new(4 * GB..5 * GB),
        ];
        let pci_ecam = [MemoryRange::new(8 * GB..9 * GB)];
        let pci_mmio = [MemoryRange::new(6 * GB..7 * GB)];

        let actual =
            resolve_memory_layout(input(6 * GB, None, &mmio, &pci_ecam, &pci_mmio, None)).unwrap();
        let expected = MemoryLayout::new(6 * GB, &mmio, &pci_ecam, &pci_mmio, None).unwrap();

        assert_eq!(actual.ram(), expected.ram());
        assert_eq!(actual.mmio(), expected.mmio());
        assert_eq!(actual.ram_size(), expected.ram_size());
        assert_eq!(actual.end_of_ram(), expected.end_of_ram());
        assert_eq!(actual.end_of_layout(), expected.end_of_layout());
    }

    #[test]
    fn numa_preserves_node_ordering_and_splitting() {
        let mmio = [MemoryRange::new(3 * GB..4 * GB)];
        let sizes = [2 * GB, 2 * GB];

        let actual =
            resolve_memory_layout(input(4 * GB, Some(&sizes), &mmio, &[], &[], None)).unwrap();
        let expected = MemoryLayout::new_with_numa(&sizes, &mmio, &[], &[], None).unwrap();

        assert_eq!(actual.ram(), expected.ram());
    }

    #[test]
    fn fixed_ranges_are_occupied_for_ram() {
        let mmio = [MemoryRange::new(GB..2 * GB)];
        let pci_ecam = [MemoryRange::new(3 * GB..3 * GB + MB)];
        let pci_mmio = [MemoryRange::new(4 * GB..5 * GB)];

        let actual =
            resolve_memory_layout(input(4 * GB, None, &mmio, &pci_ecam, &pci_mmio, None)).unwrap();

        assert_eq!(actual.probe_address(GB), Some(AddressType::Mmio));
        assert_eq!(actual.probe_address(3 * GB), Some(AddressType::PciEcam));
        assert_eq!(actual.probe_address(4 * GB), Some(AddressType::PciMmio));
        assert_eq!(actual.ram_size(), 4 * GB);
        assert!(actual.ram().iter().all(|ram| {
            !ram.range.overlaps(&mmio[0])
                && !ram.range.overlaps(&pci_ecam[0])
                && !ram.range.overlaps(&pci_mmio[0])
        }));
    }

    #[test]
    fn gb_sized_ram_request_uses_gb_chunks() {
        let mmio = [MemoryRange::new(GB + MB..GB + 2 * MB)];

        let actual = resolve_memory_layout(input(2 * GB, None, &mmio, &[], &[], None)).unwrap();

        assert_eq!(
            actual.ram(),
            &[
                MemoryRangeWithNode {
                    range: MemoryRange::new(0..GB),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(2 * GB..3 * GB),
                    vnode: 0,
                },
            ]
        );
    }

    #[test]
    fn sub_gb_numa_nodes_use_two_mb_alignment() {
        let sizes = [512 * MB, 512 * MB];

        let actual = resolve_memory_layout(input(GB, Some(&sizes), &[], &[], &[], None)).unwrap();

        assert_eq!(
            actual.ram(),
            &[
                MemoryRangeWithNode {
                    range: MemoryRange::new(0..512 * MB),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(512 * MB..GB),
                    vnode: 1,
                },
            ]
        );
    }

    #[test]
    fn vtl2_is_allocated_after_all_mmio() {
        let mmio = [MemoryRange::new(GB..2 * GB)];
        let pci_ecam = [MemoryRange::new(3 * GB..3 * GB + MB)];
        let pci_mmio = [MemoryRange::new(7 * GB..8 * GB)];

        let actual = resolve_memory_layout(input(
            4 * GB,
            None,
            &mmio,
            &pci_ecam,
            &pci_mmio,
            Some(vtl2_layout(2 * MB)),
        ))
        .unwrap();

        assert_eq!(actual.end_of_layout(), 8 * GB);
        assert_eq!(
            actual.vtl2_range(),
            Some(MemoryRange::new(8 * GB..8 * GB + 2 * MB))
        );
    }

    #[test]
    fn vtl2_does_not_change_ram_placement() {
        let mmio = [MemoryRange::new(GB..2 * GB)];

        let without_vtl2 =
            resolve_memory_layout(input(2 * GB, None, &mmio, &[], &[], None)).unwrap();
        let with_vtl2 = resolve_memory_layout(input(
            2 * GB,
            None,
            &mmio,
            &[],
            &[],
            Some(vtl2_layout(2 * MB)),
        ))
        .unwrap();

        assert_eq!(with_vtl2.ram(), without_vtl2.ram());
        assert_eq!(with_vtl2.end_of_layout(), without_vtl2.end_of_layout());
        assert_eq!(
            with_vtl2.vtl2_range(),
            Some(MemoryRange::new(3 * GB..3 * GB + 2 * MB))
        );
    }

    #[test]
    fn deterministic_for_same_inputs() {
        let mmio = [
            MemoryRange::new(GB..2 * GB),
            MemoryRange::new(5 * GB..6 * GB),
        ];
        let pci_ecam = [MemoryRange::new(3 * GB..3 * GB + MB)];
        let pci_mmio = [MemoryRange::new(7 * GB..8 * GB)];
        let sizes = [2 * GB, 3 * GB];

        let first = resolve_memory_layout(input(
            5 * GB,
            Some(&sizes),
            &mmio,
            &pci_ecam,
            &pci_mmio,
            None,
        ))
        .unwrap();
        let second = resolve_memory_layout(input(
            5 * GB,
            Some(&sizes),
            &mmio,
            &pci_ecam,
            &pci_mmio,
            None,
        ))
        .unwrap();

        assert_eq!(first.ram(), second.ram());
        assert_eq!(first.end_of_layout(), second.end_of_layout());
    }

    #[test]
    fn host_width_validation_happens_after_allocation() {
        let mmio = [MemoryRange::new(GB..4 * GB)];
        let mut config = input(3 * GB, None, &mmio, &[], &[], None);
        config.physical_address_size = 32;

        let err = resolve_memory_layout(config).unwrap_err();

        assert!(err.to_string().contains("memory layout ends at"));
    }
}
