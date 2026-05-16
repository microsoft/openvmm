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
//! The resolver owns all layout consumers: architectural reserved zones (LAPIC,
//! IOAPIC, GIC, etc.), chipset MMIO (VMBus, PIIX4 PCI BARs), PCIe
//! ECAM/BAR pools, virtio-mmio slots, ordinary RAM, VTL2 private memory, and
//! VTL2 chipset MMIO. Callers express sizing intent; the resolver places
//! everything and derives the effective MMIO gaps for [`MemoryLayout`].

use super::vm_loaders::igvm::Vtl2MemoryLayoutRequest;
use anyhow::Context;
use anyhow::bail;
use memory_range::MemoryRange;
use openvmm_defs::config::PcieMmioRangeConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use std::sync::Arc;
use vm_topology::layout::LayoutBuilder;
use vm_topology::layout::Placement;
use vm_topology::memory::MemoryLayout;
use vm_topology::memory::MemoryRangeWithNode;

const PAGE_SIZE: u64 = 4096;
const TWO_MB: u64 = 2 * 1024 * 1024;
const GB: u64 = 1024 * 1024 * 1024;

/// PCIe ECAM: 32 devices * 8 functions * 4 KiB config space = 1 MB per bus.
const PCIE_ECAM_BYTES_PER_BUS: u64 = 32 * 8 * 4096;

/// Minimum guest physical address at which an ECAM range may be placed.
///
/// The ACPI MCFG table reports the bus-0 base as
/// `ecam_range.start() - start_bus * 1 MiB`. `start_bus` is a `u8`, so up to
/// 255 MiB of headroom may be required. Rounding up to a flat 256 MiB gives a
/// single easy-to-remember invariant that works for every legal `start_bus`
/// value, independent of any individual root complex's configuration.
const PCIE_ECAM_MIN_ADDRESS: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct ResolvedMemoryLayout {
    pub memory_layout: MemoryLayout,
    pub pcie_root_complex_ranges: Vec<ResolvedPcieRootComplexRanges>,
    /// Contiguous MMIO region for all virtio-mmio device slots. Each slot is
    /// 4 KiB, indexed from the start of the region. `None` when no
    /// virtio-mmio devices are configured.
    pub virtio_mmio_region: Option<MemoryRange>,
    /// Chipset low MMIO range (below 4 GB) for VMOD/PCI0 _CRS. `None` when
    /// no VMBus / chipset MMIO is configured.
    pub chipset_low_mmio: Option<MemoryRange>,
    /// Chipset high MMIO range (above RAM) for VMOD/PCI0 _CRS. `None` when
    /// no VMBus / chipset MMIO is configured.
    pub chipset_high_mmio: Option<MemoryRange>,
    /// VTL2-private chipset MMIO range, reported to VTL2 VMBus via the device
    /// tree. `None` when VTL2 is not configured or has no chipset MMIO.
    pub vtl2_chipset_mmio: Option<MemoryRange>,
}

#[derive(Debug)]
pub(super) struct ResolvedPcieRootComplexRanges {
    pub ecam_range: MemoryRange,
    pub low_mmio: MemoryRange,
    pub high_mmio: MemoryRange,
}

pub(super) struct MemoryLayoutInput<'a> {
    /// Total VTL0 RAM size requested by the VM configuration.
    pub mem_size: u64,
    /// Optional per-vNUMA RAM budgets. When present, these must sum to
    /// `mem_size`, and request order is the vnode assignment order.
    pub numa_mem_sizes: Option<&'a [u64]>,
    /// Chipset low MMIO size (below 4 GB). This is the VMOD/PCI0 _CRS range
    /// for VMBus devices and PIIX4 PCI BARs. The address is always allocated
    /// dynamically. `0` disables the range.
    pub chipset_low_mmio_size: u64,
    /// Chipset high MMIO size (above RAM). This is the VMOD/PCI0 _CRS high
    /// range for VMBus devices. The address is always allocated dynamically.
    /// `0` disables the range.
    pub chipset_high_mmio_size: u64,
    /// VTL2-private chipset MMIO size. Placed after all VTL0-visible layout
    /// so enabling VTL2 does not move VTL0 addresses. The address is always
    /// allocated dynamically. `0` disables the range.
    pub vtl2_chipset_mmio_size: u64,
    /// PCIe root complex address-space intents. These are resolved by this
    /// worker step so front ends do not need to carve guest physical addresses.
    pub pcie_root_complexes: &'a [PcieRootComplexConfig],
    /// Number of virtio-mmio device slots to allocate in 32-bit MMIO space.
    /// A single contiguous region of `count * 4 KiB` is allocated.
    pub virtio_mmio_count: usize,
    /// Optional IGVM VTL2 private-memory request. This is allocated after all
    /// VTL0-visible RAM and MMIO and is carried separately from ordinary RAM.
    pub vtl2_layout: Option<Vtl2MemoryLayoutRequest>,
    /// Host-supported physical address width used only after allocation. The
    /// allocator computes the smallest layout it can; host fit is validation.
    pub physical_address_size: u8,
}

/// Architectural reserved zone for x86_64: LAPIC, IOAPIC, battery, TPM.
const ARCH_RESERVED_X86_64: MemoryRange = MemoryRange::new(0xFE00_0000..0x1_0000_0000);

/// Architectural reserved zone for aarch64: GIC, PL011, battery.
const ARCH_RESERVED_AARCH64: MemoryRange = MemoryRange::new(0xEF00_0000..0x1_0000_0000);

pub(super) fn resolve_memory_layout(
    input: MemoryLayoutInput<'_>,
) -> anyhow::Result<ResolvedMemoryLayout> {
    let ram_sizes = validate_ram_sizes(input.mem_size, input.numa_mem_sizes)?;

    // Chipset low and high MMIO must be paired: downstream consumers (UEFI,
    // x64 DSDT, PCAT) index `MemoryLayout::mmio()` positionally and require
    // both entries to be present. Allowing only one to be set would silently
    // produce a layout where consumers either fail late or, with VTL2
    // enabled, misinterpret the VTL2 chipset MMIO range as the high gap.
    if (input.chipset_low_mmio_size == 0) != (input.chipset_high_mmio_size == 0) {
        bail!(
            "chipset low and high MMIO must be both enabled or both disabled (low={:#x}, high={:#x})",
            input.chipset_low_mmio_size,
            input.chipset_high_mmio_size,
        );
    }

    let mut ram_ranges_by_node = vec![Vec::new(); ram_sizes.len()];
    let mut pcie_root_complex_ranges = input
        .pcie_root_complexes
        .iter()
        .map(|_| ResolvedPcieRootComplexRanges {
            ecam_range: MemoryRange::EMPTY,
            low_mmio: MemoryRange::EMPTY,
            high_mmio: MemoryRange::EMPTY,
        })
        .collect::<Vec<_>>();
    let mut vtl2_range = MemoryRange::EMPTY;
    let mut virtio_mmio_region = MemoryRange::EMPTY;
    let mut chipset_low_mmio = MemoryRange::EMPTY;
    let mut chipset_high_mmio = MemoryRange::EMPTY;
    let mut vtl2_chipset_mmio = MemoryRange::EMPTY;

    let mut builder = LayoutBuilder::new();

    // Architectural reserved zone — pinned addresses that no dynamic consumer
    // may overlap (LAPIC, IOAPIC, GIC, PL011, battery, TPM, etc.).
    let arch_reserved = if cfg!(guest_arch = "x86_64") {
        ARCH_RESERVED_X86_64
    } else {
        ARCH_RESERVED_AARCH64
    };
    builder.reserve("arch-reserved", arch_reserved);

    // Chipset low MMIO (Mmio32): VMOD/PCI0 _CRS low range for VMBus
    // devices and PIIX4 PCI BARs.
    if input.chipset_low_mmio_size != 0 {
        builder.request(
            "chipset-low-mmio",
            &mut chipset_low_mmio,
            input.chipset_low_mmio_size,
            TWO_MB,
            Placement::Mmio32,
        );
    }

    // Chipset high MMIO (Mmio64): VMOD/PCI0 _CRS high range.
    if input.chipset_high_mmio_size != 0 {
        builder.request(
            "chipset-high-mmio",
            &mut chipset_high_mmio,
            input.chipset_high_mmio_size,
            TWO_MB,
            Placement::Mmio64,
        );
    }

    for (root_complex, ranges) in input
        .pcie_root_complexes
        .iter()
        .zip(&mut pcie_root_complex_ranges)
    {
        // ECAM: always dynamically allocated below 4GB (since Linux on x86_64
        // refuses to use ECAM above 4GB unless the BIOS is of a special shape).
        // Size is derived from the bus range.
        //
        // TODO: fix the Linux loader and move this above 4GB before the layout
        // is stabilized.
        builder.request(
            format!("pcie-{}-ecam", root_complex.name),
            &mut ranges.ecam_range,
            pcie_ecam_size(root_complex)?,
            PCIE_ECAM_BYTES_PER_BUS,
            Placement::Mmio32,
        );
        // Low MMIO: 2 MB aligned.
        add_mmio_range(
            &mut builder,
            format!("pcie-{}-low-mmio", root_complex.name),
            &mut ranges.low_mmio,
            &root_complex.low_mmio,
            TWO_MB,
            Placement::Mmio32,
        )?;
        // High MMIO: 1 GB aligned. Ideally we'd align it to its actual size so
        // that the full amount is always usable for a single large BAR. But
        // that burns physical address space, which is especially limited on
        // some x86 machines.
        //
        // The downside of this approach is that the maximum mappable BAR size
        // is a function of the rest of the topology, which can create
        // reliability issues for users.
        add_mmio_range(
            &mut builder,
            format!("pcie-{}-high-mmio", root_complex.name),
            &mut ranges.high_mmio,
            &root_complex.high_mmio,
            GB,
            Placement::Mmio64,
        )?;
    }

    // Virtio-mmio: allocate one contiguous region for all slots. Each slot is
    // 4 KiB, so the region is `count * 4 KiB` placed as a single Mmio32
    // request.
    if input.virtio_mmio_count > 0 {
        builder.request(
            "virtio-mmio",
            &mut virtio_mmio_region,
            input.virtio_mmio_count as u64 * PAGE_SIZE,
            PAGE_SIZE,
            Placement::Mmio32,
        );
    }

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
        builder.ram(format!("ram{vnode}"), ram_ranges, ram_size, ram_alignment);
    }

    // VTL2 chipset MMIO is implementation-private — placed after all
    // VTL0-visible RAM/MMIO so enabling VTL2 does not move VTL0 addresses.
    if input.vtl2_chipset_mmio_size != 0 {
        builder.request(
            "vtl2-chipset-mmio",
            &mut vtl2_chipset_mmio,
            input.vtl2_chipset_mmio_size,
            TWO_MB,
            Placement::PostMmio,
        );
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

    let placed_ranges = builder
        .allocate()
        .context("allocating memory layout ranges")?;

    // Enforce the MCFG bus-0 base invariant: every ECAM range must sit at
    // `PCIE_ECAM_MIN_ADDRESS` or above. Fail fast at VM construction with a
    // clear error rather than letting an unrepresentable MCFG entry surface
    // later as a panic (debug) or silent wraparound (release).
    for (root_complex, ranges) in input
        .pcie_root_complexes
        .iter()
        .zip(&pcie_root_complex_ranges)
    {
        if ranges.ecam_range.start() < PCIE_ECAM_MIN_ADDRESS {
            bail!(
                "PCIe root complex {:?}: ECAM at {:#x} is below the {:#x} minimum",
                root_complex.name,
                ranges.ecam_range.start(),
                PCIE_ECAM_MIN_ADDRESS,
            );
        }
    }

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

    // `MemoryLayout::mmio()` is a legacy positional contract preserved here
    // exactly as callers had it pre-allocator: `[0]` = chipset low MMIO,
    // `[1]` = chipset high MMIO, and (when VTL2 is enabled) `[2]` = the
    // VTL2-private chipset MMIO range. Consumers (DSDT, Linux DT, UEFI,
    // PCAT) rely on this ordering. The architectural reserved zone and
    // virtio-mmio region were never part of this vector and remain tracked
    // separately. `MemoryLayout::mmio()` will eventually be removed.
    let mut mmio_gaps: Vec<MemoryRange> = Vec::new();
    if input.chipset_low_mmio_size != 0 {
        mmio_gaps.push(chipset_low_mmio);
    }
    if input.chipset_high_mmio_size != 0 {
        mmio_gaps.push(chipset_high_mmio);
    }
    if input.vtl2_chipset_mmio_size != 0 {
        mmio_gaps.push(vtl2_chipset_mmio);
    }

    let mut pci_ecam_gaps: Vec<MemoryRange> = Vec::new();
    pci_ecam_gaps.extend(
        pcie_root_complex_ranges
            .iter()
            .map(|ranges| ranges.ecam_range),
    );
    pci_ecam_gaps.sort();

    let mut pci_mmio_gaps: Vec<MemoryRange> = Vec::new();
    pci_mmio_gaps.extend(
        pcie_root_complex_ranges
            .iter()
            .flat_map(|ranges| [ranges.low_mmio, ranges.high_mmio]),
    );
    pci_mmio_gaps.sort();

    let memory_layout = MemoryLayout::new_from_resolved_ranges(
        ram,
        mmio_gaps,
        pci_ecam_gaps,
        pci_mmio_gaps,
        vtl2_range,
    )
    .context("validating resolved memory layout")?;

    // Host address-width validation is intentionally after allocation. The
    // layout engine is host-width independent, which keeps the layout a pure
    // function of VM configuration and avoids host differences changing guest
    // physical addresses.
    let address_space_limit = 1u64 << input.physical_address_size;
    let layout_top = placed_ranges.last().map(|r| r.range.end()).unwrap_or(0);
    if layout_top > address_space_limit {
        bail!(
            "memory layout ends at {:#x}, which exceeds the address width of {} bits",
            layout_top,
            input.physical_address_size
        );
    }

    let virtio_mmio_region = if input.virtio_mmio_count > 0 {
        Some(virtio_mmio_region)
    } else {
        None
    };

    Ok(ResolvedMemoryLayout {
        memory_layout,
        pcie_root_complex_ranges,
        virtio_mmio_region,
        chipset_low_mmio: (input.chipset_low_mmio_size != 0).then_some(chipset_low_mmio),
        chipset_high_mmio: (input.chipset_high_mmio_size != 0).then_some(chipset_high_mmio),
        vtl2_chipset_mmio: (input.vtl2_chipset_mmio_size != 0).then_some(vtl2_chipset_mmio),
    })
}

fn pcie_ecam_size(root_complex: &PcieRootComplexConfig) -> anyhow::Result<u64> {
    let bus_count = root_complex
        .end_bus
        .checked_sub(root_complex.start_bus)
        .with_context(|| {
            format!(
                "invalid PCIe bus range {}..{} for {}",
                root_complex.start_bus, root_complex.end_bus, root_complex.name
            )
        })?;

    Ok((u64::from(bus_count) + 1) * PCIE_ECAM_BYTES_PER_BUS)
}

fn add_mmio_range<'a>(
    builder: &mut LayoutBuilder<'a>,
    tag: impl Into<Arc<str>>,
    target: &'a mut MemoryRange,
    config: &PcieMmioRangeConfig,
    alignment: u64,
    placement: Placement,
) -> anyhow::Result<()> {
    let tag = tag.into();
    match config {
        PcieMmioRangeConfig::Dynamic { size } => {
            builder.request(tag, target, *size, alignment, placement);
        }
        PcieMmioRangeConfig::Fixed(range) => {
            // A fixed low-MMIO range must satisfy the Mmio32 placement contract.
            // Without this check, an above-4 GiB range would be accepted and
            // then silently truncated to 32 bits in the ARM64 PCI device tree
            // (`ranges` property uses `low_start as u32`).
            if placement == Placement::Mmio32 && range.end() > 4 * GB {
                bail!("{tag}: fixed low MMIO range {range} must end at or below 4 GiB",);
            }
            *target = *range;
            builder.fixed(tag, *range);
        }
    }
    Ok(())
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
    const DEFAULT_CHIPSET_LOW_MMIO_SIZE_X86_64: u64 = 96 * 1024 * 1024;
    const DEFAULT_CHIPSET_HIGH_MMIO_SIZE: u64 = 512 * 1024 * 1024;
    const DEFAULT_VTL2_CHIPSET_MMIO_SIZE: u64 = GB;

    fn input(
        mem_size: u64,
        numa_mem_sizes: Option<&[u64]>,
        vtl2_layout: Option<Vtl2MemoryLayoutRequest>,
    ) -> MemoryLayoutInput<'_> {
        MemoryLayoutInput {
            mem_size,
            numa_mem_sizes,
            chipset_low_mmio_size: DEFAULT_CHIPSET_LOW_MMIO_SIZE_X86_64,
            chipset_high_mmio_size: DEFAULT_CHIPSET_HIGH_MMIO_SIZE,
            vtl2_chipset_mmio_size: 0,
            pcie_root_complexes: &[],
            virtio_mmio_count: 0,
            vtl2_layout,
            physical_address_size: 46,
        }
    }

    fn resolve(input: MemoryLayoutInput<'_>) -> MemoryLayout {
        resolve_memory_layout(input).unwrap().memory_layout
    }

    fn vtl2_layout(size: u64) -> Vtl2MemoryLayoutRequest {
        Vtl2MemoryLayoutRequest {
            size,
            alignment: PAGE_SIZE,
        }
    }

    fn pcie_root_complex(
        low_mmio: PcieMmioRangeConfig,
        high_mmio: PcieMmioRangeConfig,
    ) -> PcieRootComplexConfig {
        PcieRootComplexConfig {
            index: 0,
            name: "rc0".to_string(),
            segment: 0,
            start_bus: 0,
            end_bus: 0,
            low_mmio,
            high_mmio,
            ports: Vec::new(),
        }
    }

    #[test]
    fn basic_ram_placement() {
        let actual = resolve(input(2 * GB, None, None));

        assert_eq!(actual.ram_size(), 2 * GB);
        // RAM starts at GPA 0 and fills upward.
        assert_eq!(actual.ram()[0].range.start(), 0);
    }

    #[test]
    fn ram_splits_around_arch_reserved_zone() {
        // 4 GB of RAM must split around the architectural reserved zone
        // and the chipset MMIO allocations below 4 GB.
        let actual = resolve(input(4 * GB, None, None));

        assert_eq!(actual.ram_size(), 4 * GB);
        // RAM must not overlap the architectural reserved zone.
        let reserved = ARCH_RESERVED_X86_64;
        for ram in actual.ram() {
            assert!(
                !ram.range.overlaps(&reserved),
                "RAM {:?} overlaps reserved {:?}",
                ram.range,
                reserved
            );
        }
    }

    #[test]
    fn numa_preserves_node_ordering() {
        let sizes = [2 * GB, 2 * GB];

        let actual = resolve(input(4 * GB, Some(&sizes), None));

        // First vnode's RAM starts at 0.
        assert_eq!(actual.ram()[0].vnode, 0);
        assert_eq!(actual.ram()[0].range.start(), 0);
        // All RAM accounts for 4 GB total.
        assert_eq!(actual.ram_size(), 4 * GB);
    }

    #[test]
    fn chipset_mmio_is_resolved() {
        let result = resolve_memory_layout(input(2 * GB, None, None)).unwrap();

        let low = result
            .chipset_low_mmio
            .expect("should have low chipset MMIO");
        let high = result
            .chipset_high_mmio
            .expect("should have high chipset MMIO");
        assert_eq!(low.len(), DEFAULT_CHIPSET_LOW_MMIO_SIZE_X86_64);
        assert_eq!(high.len(), DEFAULT_CHIPSET_HIGH_MMIO_SIZE);
        assert!(low.end() <= 4 * GB, "low chipset MMIO should be below 4 GB");
        assert!(
            high.start() >= 2 * GB,
            "high chipset MMIO should be above RAM"
        );
    }

    #[test]
    fn pcie_dynamic_intents_are_resolved() {
        let root_complexes = [pcie_root_complex(
            PcieMmioRangeConfig::Dynamic { size: 64 * MB },
            PcieMmioRangeConfig::Dynamic { size: GB },
        )];
        let mut config = input(2 * GB, None, None);
        config.pcie_root_complexes = &root_complexes;

        let actual = resolve_memory_layout(config).unwrap();
        let ranges = &actual.pcie_root_complex_ranges[0];

        assert!(
            ranges.ecam_range.end() <= 4 * GB,
            "ECAM should be below 4 GB"
        );
        assert_eq!(ranges.low_mmio.len(), 64 * MB);
        assert_eq!(ranges.high_mmio.len(), GB);
        assert_eq!(
            actual
                .memory_layout
                .probe_address(ranges.ecam_range.start()),
            Some(AddressType::PciEcam)
        );
        assert_eq!(
            actual.memory_layout.probe_address(ranges.low_mmio.start()),
            Some(AddressType::PciMmio)
        );
        assert_eq!(
            actual.memory_layout.probe_address(ranges.high_mmio.start()),
            Some(AddressType::PciMmio)
        );
    }

    #[test]
    fn sub_gb_numa_nodes_use_two_mb_alignment() {
        let sizes = [512 * MB, 512 * MB];

        let actual = resolve(input(GB, Some(&sizes), None));

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
        let actual = resolve(input(4 * GB, None, Some(vtl2_layout(2 * MB))));

        assert!(actual.vtl2_range().is_some());
        let vtl2 = actual.vtl2_range().unwrap();
        assert_eq!(vtl2.len(), 2 * MB);
        // VTL2 should be after all other allocations.
        for ram in actual.ram() {
            assert!(vtl2.start() >= ram.range.end());
        }
    }

    #[test]
    fn vtl2_does_not_change_ram_placement() {
        let without_vtl2 = resolve(input(2 * GB, None, None));
        let with_vtl2 = resolve(input(2 * GB, None, Some(vtl2_layout(2 * MB))));

        assert_eq!(with_vtl2.ram(), without_vtl2.ram());
    }

    #[test]
    fn deterministic_for_same_inputs() {
        let sizes = [2 * GB, 3 * GB];

        let first = resolve(input(5 * GB, Some(&sizes), None));
        let second = resolve(input(5 * GB, Some(&sizes), None));

        assert_eq!(first.ram(), second.ram());
        assert_eq!(first.end_of_layout(), second.end_of_layout());
    }

    #[test]
    fn host_width_validation_happens_after_allocation() {
        // Use enough RAM that the layout (RAM + chipset high MMIO + arch
        // reserved zone) exceeds 32 bits.
        let mut config = input(4 * GB, None, None);
        config.physical_address_size = 32;

        let err = resolve_memory_layout(config).unwrap_err();

        assert!(err.to_string().contains("memory layout ends at"));
    }

    #[test]
    fn virtio_mmio_slots_are_allocated_in_mmio32() {
        let mut config = input(2 * GB, None, None);
        config.virtio_mmio_count = 3;

        let result = resolve_memory_layout(config).unwrap();

        let region = result
            .virtio_mmio_region
            .expect("should have virtio-mmio region");
        assert_eq!(region.len(), 3 * PAGE_SIZE);
        assert!(region.end() <= 4 * GB, "virtio-mmio should be below 4 GB");
    }

    #[test]
    fn virtio_mmio_does_not_move_ram() {
        let without = resolve(input(2 * GB, None, None));
        let mut config = input(2 * GB, None, None);
        config.virtio_mmio_count = 2;
        let with = resolve_memory_layout(config).unwrap();

        assert_eq!(with.memory_layout.ram(), without.ram());
    }

    #[test]
    fn zero_virtio_mmio_produces_no_region() {
        let config = input(2 * GB, None, None);

        let result = resolve_memory_layout(config).unwrap();

        assert!(result.virtio_mmio_region.is_none());
    }

    #[test]
    fn vtl2_chipset_mmio_is_post_mmio() {
        let mut config = input(2 * GB, None, None);
        config.vtl2_chipset_mmio_size = DEFAULT_VTL2_CHIPSET_MMIO_SIZE;

        let result = resolve_memory_layout(config).unwrap();

        let vtl2_mmio = result
            .vtl2_chipset_mmio
            .expect("should have VTL2 chipset MMIO");
        assert_eq!(vtl2_mmio.len(), DEFAULT_VTL2_CHIPSET_MMIO_SIZE);
        // VTL2 chipset MMIO should be after all VTL0-visible ranges.
        let chipset_high = result
            .chipset_high_mmio
            .expect("should have high chipset MMIO");
        assert!(
            vtl2_mmio.start() >= chipset_high.end(),
            "VTL2 chipset MMIO should be after VTL0 high MMIO"
        );
    }

    #[test]
    fn vtl2_chipset_mmio_does_not_move_vtl0_layout() {
        let without = resolve(input(2 * GB, None, None));
        let mut config = input(2 * GB, None, None);
        config.vtl2_chipset_mmio_size = DEFAULT_VTL2_CHIPSET_MMIO_SIZE;
        let with = resolve_memory_layout(config).unwrap();

        assert_eq!(with.memory_layout.ram(), without.ram());
    }

    #[test]
    fn no_chipset_mmio_when_none() {
        let mut config = input(2 * GB, None, None);
        config.chipset_low_mmio_size = 0;
        config.chipset_high_mmio_size = 0;

        let result = resolve_memory_layout(config).unwrap();

        assert!(result.chipset_low_mmio.is_none());
        assert!(result.chipset_high_mmio.is_none());
    }

    #[test]
    fn asymmetric_chipset_mmio_is_rejected() {
        let mut config = input(2 * GB, None, None);
        config.chipset_high_mmio_size = 0;
        let err = resolve_memory_layout(config).unwrap_err();
        assert!(
            err.to_string().contains("both enabled or both disabled"),
            "unexpected error: {err}"
        );

        let mut config = input(2 * GB, None, None);
        config.chipset_low_mmio_size = 0;
        let err = resolve_memory_layout(config).unwrap_err();
        assert!(
            err.to_string().contains("both enabled or both disabled"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn fixed_low_mmio_above_4gb_is_rejected() {
        let root_complexes = [pcie_root_complex(
            // A 1 GiB fixed low MMIO range placed above 4 GiB violates the
            // Mmio32 placement contract.
            PcieMmioRangeConfig::Fixed(MemoryRange::new(5 * GB..6 * GB)),
            PcieMmioRangeConfig::Dynamic { size: GB },
        )];
        let mut config = input(2 * GB, None, None);
        config.pcie_root_complexes = &root_complexes;
        let err = resolve_memory_layout(config).unwrap_err();
        assert!(
            err.to_string().contains("must end at or below 4 GiB"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ecam_below_256mb_is_rejected() {
        // Force ECAM placement below 256 MiB by reserving most of the 32-bit
        // MMIO window for low_mmio. The Mmio32 zone is ~4064 MiB on x86_64
        // and ~3824 MiB on aarch64 (the per-arch reserved zone differs), so
        // the low_mmio request is sized per-arch to land ECAM around 127 MiB
        // in both cases. The resolver must bail because MCFG cannot
        // represent a bus-0 base below the ECAM start.
        let low_mmio_size = if cfg!(guest_arch = "x86_64") {
            3840 * MB
        } else {
            3600 * MB
        };
        let root_complexes = [pcie_root_complex(
            PcieMmioRangeConfig::Dynamic {
                size: low_mmio_size,
            },
            PcieMmioRangeConfig::Dynamic { size: GB },
        )];
        let mut config = input(2 * GB, None, None);
        config.pcie_root_complexes = &root_complexes;

        let err = resolve_memory_layout(config).unwrap_err();

        assert!(err.to_string().contains("ECAM"), "unexpected error: {err}");
    }
}
