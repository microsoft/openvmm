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
use cxl_spec::spec::CXL_HOST_BRIDGE_COMPONENT_REGISTERS_SIZE_BYTES;
use cxl_spec::spec::CXL_HPA_ALIGNMENT;
use memory_range::MemoryRange;
use openvmm_defs::config::PcieIommuConfig;
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

/// SMMUv3 MMIO region size: two 64 KiB pages (page 0 + page 1).
const SMMU_SIZE: u64 = 0x2_0000;

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

/// Resolved chipset MMIO ranges produced by the memory layout engine.
#[derive(Debug, Copy, Clone)]
pub(crate) struct ChipsetMmioRanges {
    /// Chipset low MMIO range (below 4 GB) for VMOD/PCI0 _CRS. Always at
    /// least the architectural reserved zone (LAPIC, IOAPIC, TPM, ...).
    pub low: MemoryRange,
    /// Chipset high MMIO range (above RAM) for VMOD/PCI0 _CRS. `EMPTY` when
    /// no chipset high MMIO is configured.
    pub high: MemoryRange,
    /// VTL2-private chipset MMIO range, reported to VTL2 VMBus via the device
    /// tree. `EMPTY` when VTL2 is not configured or has no chipset MMIO.
    pub vtl2: MemoryRange,
}

#[derive(Debug)]
pub(super) struct ResolvedMemoryLayout {
    pub memory_layout: MemoryLayout,
    pub pcie_root_complex_ranges: Vec<ResolvedPcieRootComplexRanges>,
    /// Contiguous MMIO region for all virtio-mmio device slots. Each slot is
    /// 4 KiB, indexed from the start of the region. `EMPTY` when no
    /// virtio-mmio devices are configured.
    pub virtio_mmio_region: MemoryRange,
    /// Resolved chipset MMIO ranges.
    pub chipset_mmio: ChipsetMmioRanges,
    /// Resolved VTL2 framebuffer GPA base. `None` when VTL2 graphics is not
    /// configured.
    pub vtl2_framebuffer_gpa_base: Option<u64>,
    /// Resolved MMIO ranges for the VM's IOMMU, if any. At most one IOMMU
    /// type is configured per VM, so this is keyed by type rather than stored
    /// as three independent fields.
    pub iommu_ranges: ResolvedIommuRanges,
}

/// Resolved MMIO ranges for the VM's IOMMU, keyed by IOMMU type.
///
/// A VM has at most one IOMMU type, so encoding the ranges as an enum makes the
/// "only one kind" invariant structural rather than relying on three separate
/// fields that callers must remember are mutually exclusive.
#[derive(Debug, Default)]
pub(super) enum ResolvedIommuRanges {
    /// No IOMMU is configured.
    #[default]
    None,
    /// Arm SMMUv3 instances, one `SMMU_SIZE`-byte range each.
    #[cfg_attr(not(guest_arch = "aarch64"), expect(dead_code))]
    Smmu(Vec<MemoryRange>),
    /// AMD IOMMU instances, one 16 KiB range each.
    #[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
    AmdVi(Vec<MemoryRange>),
    /// Intel VT-d units, one 4 KiB range each.
    #[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
    IntelVtd(Vec<MemoryRange>),
}

#[derive(Debug)]
pub(super) struct ResolvedPcieRootComplexRanges {
    pub ecam_range: MemoryRange,
    pub low_mmio: MemoryRange,
    pub high_mmio: MemoryRange,
    pub chbcr_range: MemoryRange,
    pub hdm_range: MemoryRange,
}

pub(super) struct MemoryLayoutInput<'a> {
    /// Per-NUMA-node RAM sizes. Every VM has at least one node (a
    /// single-node VM is `&[total_size]`). Entries may be zero for
    /// memory-less nodes (e.g. device-only NUMA nodes). The request
    /// order is the vnode assignment order.
    pub node_mem_sizes: &'a [u64],
    /// Chipset MMIO sizing from the manifest builder.
    pub layout: vmm_core_defs::LayoutConfig,
    /// PCIe root complex address-space intents. These are resolved by this
    /// worker step so front ends do not need to carve guest physical addresses.
    pub pcie_root_complexes: &'a [PcieRootComplexConfig],
    /// Number of virtio-mmio device slots to allocate in 32-bit MMIO space.
    /// A single contiguous region of `count * 4 KiB` is allocated.
    pub virtio_mmio_count: usize,
    /// Optional IGVM VTL2 private-memory request. This is allocated after all
    /// VTL0-visible RAM and MMIO and is carried separately from ordinary RAM.
    pub vtl2_layout: Option<Vtl2MemoryLayoutRequest>,
    /// Minimum guest physical address for ordinary RAM. When nonzero, the
    /// range `0..ram_start_address` is reserved so RAM is placed above it.
    /// This is used on aarch64 Linux direct boot to avoid the low GPA region
    /// that conflicts with iommufd IOVA reservations.
    pub ram_start_address: u64,
    /// Size in bytes of the VTL2 framebuffer mapping. When non-zero, a
    /// `PostMmio` allocation is created and the resolved GPA is returned in
    /// `ResolvedMemoryLayout::vtl2_framebuffer_gpa_base`.
    pub vtl2_framebuffer_size: u64,
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
    validate_node_mem_sizes(input.node_mem_sizes)?;

    let mut ram_ranges_by_node = vec![Vec::new(); input.node_mem_sizes.len()];
    let mut pcie_root_complex_ranges = input
        .pcie_root_complexes
        .iter()
        .map(|_| ResolvedPcieRootComplexRanges {
            ecam_range: MemoryRange::EMPTY,
            low_mmio: MemoryRange::EMPTY,
            high_mmio: MemoryRange::EMPTY,
            chbcr_range: MemoryRange::EMPTY,
            hdm_range: MemoryRange::EMPTY,
        })
        .collect::<Vec<_>>();

    let mut builder = LayoutBuilder::new();

    // Chipset low MMIO (Mmio32): a fixed window pinned to the top of 32-bit
    // address space, advertised to firmware as `\_SB.VMOD._CRS`. Always at
    // least the architectural reserved zone (LAPIC, IOAPIC, TPM, ...) so
    // guests can arbitrate fixed-address children like TPM2 against this
    // window; the caller-requested size may extend it lower.
    // Reserve low addresses so RAM starts above `ram_start_address`. This is
    // used on aarch64 Linux direct boot to skip the 128 MiB–129 MiB IOVA
    // region that iommufd reserves for the host MSI doorbell.
    if input.ram_start_address > 0 {
        builder.reserve("low-ram-gap", MemoryRange::new(0..input.ram_start_address));
    }

    let arch_reserved = if cfg!(guest_arch = "x86_64") {
        ARCH_RESERVED_X86_64
    } else {
        ARCH_RESERVED_AARCH64
    };
    let four_gb = 4 * GB;
    let low_mmio_size = u64::from(input.layout.chipset_low_mmio_size)
        .next_multiple_of(0x1000)
        .max(arch_reserved.len());
    let chipset_low_mmio = MemoryRange::new(four_gb - low_mmio_size..four_gb);
    builder.fixed("chipset-low-mmio", chipset_low_mmio);

    // Chipset high MMIO (Mmio64): VMOD/PCI0 _CRS high range.
    // When no high MMIO is requested, use a zero-length range at 4GB so that
    // the range sorts after the low MMIO gap (rather than at address 0).
    let mut chipset_high_mmio = MemoryRange::new(four_gb..four_gb);
    if input.layout.chipset_high_mmio_size != 0 {
        builder.request(
            "chipset-high-mmio",
            &mut chipset_high_mmio,
            input.layout.chipset_high_mmio_size,
            TWO_MB,
            Placement::Mmio64,
        );
    }

    // Group root complexes by PCI segment so that RCs sharing a segment get a
    // single contiguous ECAM block. This ensures the MCFG bus-0 base address
    // is consistent for all RCs in the same segment.
    struct SegmentEcam {
        segment: u16,
        min_bus: u8,
        max_bus: u8,
        range: MemoryRange,
    }
    let mut segment_ecams: Vec<SegmentEcam> = Vec::new();
    for rc in input.pcie_root_complexes {
        if let Some(entry) = segment_ecams.iter_mut().find(|e| e.segment == rc.segment) {
            entry.min_bus = entry.min_bus.min(rc.start_bus);
            entry.max_bus = entry.max_bus.max(rc.end_bus);
        } else {
            segment_ecams.push(SegmentEcam {
                segment: rc.segment,
                min_bus: rc.start_bus,
                max_bus: rc.end_bus,
                range: MemoryRange::EMPTY,
            });
        }
    }

    // ECAM: dynamically allocated above 4GB, keeping the low MMIO window free
    // for devices that require 32-bit addressing.
    for se in &mut segment_ecams {
        let bus_count = u64::from(se.max_bus - se.min_bus) + 1;
        builder.request(
            format!("pcie-seg{}-ecam", se.segment),
            &mut se.range,
            bus_count * PCIE_ECAM_BYTES_PER_BUS,
            PCIE_ECAM_BYTES_PER_BUS,
            Placement::Mmio64,
        );
    }

    for (root_complex, ranges) in input
        .pcie_root_complexes
        .iter()
        .zip(&mut pcie_root_complex_ranges)
    {
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

        if let Some(cxl) = &root_complex.cxl {
            let hdm_config = PcieMmioRangeConfig::Dynamic { size: cxl.hdm_size };
            add_mmio_range(
                &mut builder,
                format!("pcie-{}-cxl-hdm", root_complex.name),
                &mut ranges.hdm_range,
                &hdm_config,
                CXL_HPA_ALIGNMENT,
                Placement::Mmio64,
            )?;

            let chbcr_config = PcieMmioRangeConfig::Dynamic {
                size: CXL_HOST_BRIDGE_COMPONENT_REGISTERS_SIZE_BYTES,
            };
            add_mmio_range(
                &mut builder,
                format!("pcie-{}-cxl-chbcr", root_complex.name),
                &mut ranges.chbcr_range,
                &chbcr_config,
                CXL_HOST_BRIDGE_COMPONENT_REGISTERS_SIZE_BYTES,
                Placement::Mmio64,
            )?;
        }
    }

    // Virtio-mmio: allocate one contiguous region for all slots. Each slot is
    // 4 KiB, so the region is `count * 4 KiB` placed as a single Mmio32
    // request.
    let mut virtio_mmio_region = MemoryRange::EMPTY;
    if input.virtio_mmio_count > 0 {
        builder.request(
            "virtio-mmio",
            &mut virtio_mmio_region,
            input.virtio_mmio_count as u64 * PAGE_SIZE,
            PAGE_SIZE,
            Placement::Mmio32,
        );
    }

    // IOMMU: a VM has at most one IOMMU type, so determine it once and
    // allocate one MMIO region per configured instance, placed below 4 GiB
    // alongside other system devices. Geometry is per-type; the ranges are
    // collapsed into `ResolvedIommuRanges` after allocation.
    let iommu_kind = input
        .pcie_root_complexes
        .iter()
        .find_map(|rc| rc.iommu.clone());
    let iommu_count = input
        .pcie_root_complexes
        .iter()
        .filter(|rc| rc.iommu.is_some())
        .count();
    let mut iommu_ranges = vec![MemoryRange::EMPTY; iommu_count];
    if let Some(kind) = &iommu_kind {
        assert!(
            input
                .pcie_root_complexes
                .iter()
                .filter_map(|rc| rc.iommu.as_ref())
                .all(|other| std::mem::discriminant(other) == std::mem::discriminant(kind)),
            "all configured IOMMUs must be the same type"
        );
        let (name, size) = match kind {
            // SMMUv3: 128 KiB region (two 64 KiB pages).
            PcieIommuConfig::Smmu { .. } => ("smmu", SMMU_SIZE),
            // AMD IOMMU: 16 KiB per AMD IOMMU spec §3.4.
            PcieIommuConfig::AmdVi => ("amd-iommu", 0x4000),
            // Intel VT-d: 4 KiB per VT-d spec.
            PcieIommuConfig::IntelVtd => ("intel-vtd", 0x1000),
        };
        for (idx, range) in iommu_ranges.iter_mut().enumerate() {
            builder.request(
                format!("{name}-{idx}"),
                range,
                size,
                size,
                Placement::Mmio32,
            );
        }
    }

    // RAM request order is part of the NUMA compatibility contract: the first
    // request maps to vnode 0, the second to vnode 1, and so on. Memory-less
    // nodes (size 0) are skipped so the layout allocator never sees a
    // zero-size request. For GB-sized nodes, use GB alignment so holes do not
    // create sub-GB RAM chunks. For sub-GB nodes, use 2 MB alignment to avoid
    // wasting a full GB of address space per small node.
    for (vnode, (ram_ranges, &ram_size)) in ram_ranges_by_node
        .iter_mut()
        .zip(input.node_mem_sizes)
        .enumerate()
    {
        if ram_size == 0 {
            continue;
        }
        let ram_alignment = if ram_size < GB { TWO_MB } else { GB };
        builder.ram(format!("ram{vnode}"), ram_ranges, ram_size, ram_alignment);
    }

    // VTL2 chipset MMIO is implementation-private — placed after all
    // VTL0-visible RAM/MMIO so enabling VTL2 does not move VTL0 addresses.
    let mut vtl2_chipset_mmio = MemoryRange::EMPTY;
    if input.layout.vtl2_chipset_mmio_size != 0 {
        builder.request(
            "vtl2-chipset-mmio",
            &mut vtl2_chipset_mmio,
            input.layout.vtl2_chipset_mmio_size,
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
    let mut vtl2_range = MemoryRange::EMPTY;
    if let Some(vtl2_layout) = input.vtl2_layout {
        builder.request(
            "vtl2",
            &mut vtl2_range,
            vtl2_layout.size,
            vtl2_layout.alignment,
            Placement::PostMmio,
        );
    }

    // VTL2 framebuffer: a page-aligned PostMmio allocation so the GPA does
    // not overlap RAM or any MMIO range.
    let mut vtl2_framebuffer_range = MemoryRange::EMPTY;
    if input.vtl2_framebuffer_size != 0 {
        builder.request(
            "vtl2-framebuffer",
            &mut vtl2_framebuffer_range,
            input.vtl2_framebuffer_size,
            PAGE_SIZE,
            Placement::PostMmio,
        );
    }

    let placed_ranges = builder
        .allocate()
        .context("allocating memory layout ranges")?;

    // Collapse the allocated IOMMU ranges into the type-keyed enum. The Vec
    // could not be moved earlier because the builder borrowed its elements
    // until `allocate`.
    let iommu_ranges = match iommu_kind {
        None => ResolvedIommuRanges::None,
        Some(PcieIommuConfig::Smmu { .. }) => ResolvedIommuRanges::Smmu(iommu_ranges),
        Some(PcieIommuConfig::AmdVi) => ResolvedIommuRanges::AmdVi(iommu_ranges),
        Some(PcieIommuConfig::IntelVtd) => ResolvedIommuRanges::IntelVtd(iommu_ranges),
    };

    // Subdivide per-segment ECAM blocks into per-RC sub-ranges.
    for (root_complex, ranges) in input
        .pcie_root_complexes
        .iter()
        .zip(&mut pcie_root_complex_ranges)
    {
        let se = segment_ecams
            .iter()
            .find(|e| e.segment == root_complex.segment)
            .expect("segment must exist");
        let offset = u64::from(root_complex.start_bus - se.min_bus) * PCIE_ECAM_BYTES_PER_BUS;
        let size = (u64::from(root_complex.end_bus - root_complex.start_bus) + 1)
            * PCIE_ECAM_BYTES_PER_BUS;
        ranges.ecam_range =
            MemoryRange::new(se.range.start() + offset..se.range.start() + offset + size);
    }

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

    // `MemoryLayout::mmio()` is a positional contract: `[0]` = chipset low
    // MMIO, `[1]` = chipset high MMIO, and (when VTL2 is enabled) `[2]` =
    // the VTL2-private chipset MMIO range. Consumers (DSDT, Linux DT, UEFI,
    // PCAT) rely on this ordering. Entries may be `MemoryRange::EMPTY` when
    // the corresponding range is not configured; the positional index is
    // what matters, not the presence of a non-empty range.
    let mut mmio_gaps: Vec<MemoryRange> = vec![chipset_low_mmio, chipset_high_mmio];
    if !vtl2_chipset_mmio.is_empty() {
        mmio_gaps.push(vtl2_chipset_mmio);
    }

    let mut pci_ecam_gaps: Vec<MemoryRange> = Vec::new();
    pci_ecam_gaps.extend(segment_ecams.iter().map(|se| se.range));
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

    Ok(ResolvedMemoryLayout {
        memory_layout,
        pcie_root_complex_ranges,
        virtio_mmio_region,
        chipset_mmio: ChipsetMmioRanges {
            low: chipset_low_mmio,
            high: chipset_high_mmio,
            vtl2: vtl2_chipset_mmio,
        },
        vtl2_framebuffer_gpa_base: if vtl2_framebuffer_range.is_empty() {
            None
        } else {
            Some(vtl2_framebuffer_range.start())
        },
        iommu_ranges,
    })
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

/// Validate per-node memory sizes. Zero-size entries (memory-less nodes) are
/// allowed. The total must be nonzero and all non-zero sizes must be
/// page-aligned.
fn validate_node_mem_sizes(sizes: &[u64]) -> anyhow::Result<()> {
    if sizes.is_empty() {
        bail!("empty node memory sizes (every VM needs at least one NUMA node)");
    }

    let total: u64 = sizes
        .iter()
        .copied()
        .try_fold(0u64, |acc, s| acc.checked_add(s))
        .context("node memory sizes overflow")?;
    if total == 0 {
        bail!("total RAM size is zero");
    }

    for (i, &size) in sizes.iter().enumerate() {
        if size != 0 && !size.is_multiple_of(PAGE_SIZE) {
            bail!("NUMA node {i} memory size {size:#x} is not page-aligned");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_topology::memory::AddressType;

    const MB: u64 = 1024 * 1024;
    // Match the production defaults from `vm_manifest_builder`.
    #[cfg(guest_arch = "x86_64")]
    const DEFAULT_CHIPSET_LOW_MMIO_SIZE: u32 = 128 * 1024 * 1024;
    #[cfg(guest_arch = "aarch64")]
    const DEFAULT_CHIPSET_LOW_MMIO_SIZE: u32 = 512 * 1024 * 1024;
    #[cfg(guest_arch = "x86_64")]
    const ARCH_RESERVED: MemoryRange = ARCH_RESERVED_X86_64;
    #[cfg(guest_arch = "aarch64")]
    const ARCH_RESERVED: MemoryRange = ARCH_RESERVED_AARCH64;
    const DEFAULT_CHIPSET_HIGH_MMIO_SIZE: u64 = 512 * 1024 * 1024;
    const DEFAULT_VTL2_CHIPSET_MMIO_SIZE: u64 = GB;

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
            vtl2_layout,
            ram_start_address: 0,
            vtl2_framebuffer_size: 0,
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
            cxl: None,
            iommu: None,
            vnode: None,
            preserve_bars: false,
        }
    }

    #[test]
    fn basic_ram_placement() {
        let actual = resolve(input(&[2 * GB], None));

        assert_eq!(actual.ram_size(), 2 * GB);
        // RAM starts at GPA 0 and fills upward.
        assert_eq!(actual.ram()[0].range.start(), 0);
    }

    #[test]
    fn ram_splits_around_arch_reserved_zone() {
        // 4 GB of RAM must split around the architectural reserved zone
        // and the chipset MMIO allocations below 4 GB.
        let actual = resolve(input(&[4 * GB], None));

        assert_eq!(actual.ram_size(), 4 * GB);
        // RAM must not overlap the architectural reserved zone.
        let reserved = ARCH_RESERVED;
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

        let actual = resolve(input(&sizes, None));

        // First vnode's RAM starts at 0.
        assert_eq!(actual.ram()[0].vnode, 0);
        assert_eq!(actual.ram()[0].range.start(), 0);
        // All RAM accounts for 4 GB total.
        assert_eq!(actual.ram_size(), 4 * GB);
    }

    #[test]
    fn chipset_mmio_is_resolved() {
        let result = resolve_memory_layout(input(&[2 * GB], None)).unwrap();

        let low = result.chipset_mmio.low;
        let high = result.chipset_mmio.high;
        assert_eq!(low.len(), DEFAULT_CHIPSET_LOW_MMIO_SIZE as u64);
        assert_eq!(high.len(), DEFAULT_CHIPSET_HIGH_MMIO_SIZE);
        // Chipset low MMIO is pinned to end at 4 GiB and must fully contain
        // the architectural reserved zone (LAPIC, IOAPIC, TPM, ...).
        assert_eq!(low.end(), 4 * GB);
        assert!(low.contains(&ARCH_RESERVED));
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
        let mut config = input(&[2 * GB], None);
        config.pcie_root_complexes = &root_complexes;

        let actual = resolve_memory_layout(config).unwrap();
        let ranges = &actual.pcie_root_complex_ranges[0];

        assert!(
            ranges.ecam_range.start() >= 4 * GB,
            "ECAM should be above 4 GB"
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
    fn shared_segment_gets_contiguous_ecam() {
        // Two root complexes on the same segment with disjoint bus ranges
        // must get ECAM sub-ranges within a single contiguous block, so
        // that the MCFG bus-0 base address is the same for both.
        let root_complexes = [
            PcieRootComplexConfig {
                index: 0,
                name: "rc0".to_string(),
                segment: 0,
                start_bus: 0,
                end_bus: 15,
                low_mmio: PcieMmioRangeConfig::Dynamic { size: 32 * MB },
                high_mmio: PcieMmioRangeConfig::Dynamic { size: GB },
                ports: Vec::new(),
                cxl: None,
                iommu: None,
                vnode: None,
                preserve_bars: false,
            },
            PcieRootComplexConfig {
                index: 1,
                name: "rc1".to_string(),
                segment: 0,
                start_bus: 16,
                end_bus: 31,
                low_mmio: PcieMmioRangeConfig::Dynamic { size: 32 * MB },
                high_mmio: PcieMmioRangeConfig::Dynamic { size: GB },
                ports: Vec::new(),
                cxl: None,
                iommu: None,
                vnode: None,
                preserve_bars: false,
            },
        ];
        let mut config = input(&[2 * GB], None);
        config.pcie_root_complexes = &root_complexes;

        let actual = resolve_memory_layout(config).unwrap();
        let r0 = &actual.pcie_root_complex_ranges[0];
        let r1 = &actual.pcie_root_complex_ranges[1];

        // rc0 ends exactly where rc1 starts (contiguous).
        assert_eq!(r0.ecam_range.end(), r1.ecam_range.start());

        // Both derive the same MCFG bus-0 base.
        let bus0_base_r0 = r0.ecam_range.start()
            - u64::from(root_complexes[0].start_bus) * PCIE_ECAM_BYTES_PER_BUS;
        let bus0_base_r1 = r1.ecam_range.start()
            - u64::from(root_complexes[1].start_bus) * PCIE_ECAM_BYTES_PER_BUS;
        assert_eq!(bus0_base_r0, bus0_base_r1);
    }

    #[test]
    fn full_bus_range_ecam_does_not_overflow() {
        // A single RC spanning buses 0..255 requires (255 - 0 + 1) = 256
        // buses. The bus count must be computed in u64, not u8, to avoid
        // overflow.
        let root_complexes = [PcieRootComplexConfig {
            index: 0,
            name: "rc0".to_string(),
            segment: 0,
            start_bus: 0,
            end_bus: 255,
            low_mmio: PcieMmioRangeConfig::Dynamic { size: 64 * MB },
            high_mmio: PcieMmioRangeConfig::Dynamic { size: GB },
            ports: Vec::new(),
            cxl: None,
            iommu: None,
            vnode: None,
            preserve_bars: false,
        }];
        let mut config = input(&[2 * GB], None);
        config.pcie_root_complexes = &root_complexes;

        let actual = resolve_memory_layout(config).unwrap();
        let ranges = &actual.pcie_root_complex_ranges[0];
        assert_eq!(ranges.ecam_range.len(), 256 * PCIE_ECAM_BYTES_PER_BUS);
    }

    #[test]
    fn sub_gb_numa_nodes_use_two_mb_alignment() {
        let sizes = [512 * MB, 512 * MB];

        let actual = resolve(input(&sizes, None));

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
        let actual = resolve(input(&[4 * GB], Some(vtl2_layout(2 * MB))));

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
        let without_vtl2 = resolve(input(&[2 * GB], None));
        let with_vtl2 = resolve(input(&[2 * GB], Some(vtl2_layout(2 * MB))));

        assert_eq!(with_vtl2.ram(), without_vtl2.ram());
    }

    #[test]
    fn deterministic_for_same_inputs() {
        let sizes = [2 * GB, 3 * GB];

        let first = resolve(input(&sizes, None));
        let second = resolve(input(&sizes, None));

        assert_eq!(first.ram(), second.ram());
        assert_eq!(first.end_of_layout(), second.end_of_layout());
    }

    #[test]
    fn host_width_validation_happens_after_allocation() {
        // Use enough RAM that the layout (RAM + chipset high MMIO + arch
        // reserved zone) exceeds 32 bits.
        let mut config = input(&[4 * GB], None);
        config.physical_address_size = 32;

        let err = resolve_memory_layout(config).unwrap_err();

        assert!(err.to_string().contains("memory layout ends at"));
    }

    #[test]
    fn virtio_mmio_slots_are_allocated_in_mmio32() {
        let mut config = input(&[2 * GB], None);
        config.virtio_mmio_count = 3;

        let result = resolve_memory_layout(config).unwrap();

        let region = result.virtio_mmio_region;
        assert_eq!(region.len(), 3 * PAGE_SIZE);
        assert!(region.end() <= 4 * GB, "virtio-mmio should be below 4 GB");
    }

    #[test]
    fn virtio_mmio_does_not_move_ram() {
        let without = resolve(input(&[2 * GB], None));
        let mut config = input(&[2 * GB], None);
        config.virtio_mmio_count = 2;
        let with = resolve_memory_layout(config).unwrap();

        assert_eq!(with.memory_layout.ram(), without.ram());
    }

    #[test]
    fn zero_virtio_mmio_produces_no_region() {
        let config = input(&[2 * GB], None);

        let result = resolve_memory_layout(config).unwrap();

        assert!(result.virtio_mmio_region.is_empty());
    }

    #[test]
    fn vtl2_chipset_mmio_is_post_mmio() {
        let mut config = input(&[2 * GB], None);
        config.layout.vtl2_chipset_mmio_size = DEFAULT_VTL2_CHIPSET_MMIO_SIZE;

        let result = resolve_memory_layout(config).unwrap();

        let vtl2_mmio = result.chipset_mmio.vtl2;
        assert_eq!(vtl2_mmio.len(), DEFAULT_VTL2_CHIPSET_MMIO_SIZE);
        // VTL2 chipset MMIO should be after all VTL0-visible ranges.
        let chipset_high = result.chipset_mmio.high;
        assert!(
            vtl2_mmio.start() >= chipset_high.end(),
            "VTL2 chipset MMIO should be after VTL0 high MMIO"
        );
    }

    #[test]
    fn vtl2_chipset_mmio_does_not_move_vtl0_layout() {
        let without = resolve(input(&[2 * GB], None));
        let mut config = input(&[2 * GB], None);
        config.layout.vtl2_chipset_mmio_size = DEFAULT_VTL2_CHIPSET_MMIO_SIZE;
        let with = resolve_memory_layout(config).unwrap();

        assert_eq!(with.memory_layout.ram(), without.ram());
    }

    #[test]
    fn disabled_chipset_mmio_still_reports_arch_reserved() {
        // Even when the caller does not request any chipset MMIO, the
        // architectural reserved zone (LAPIC, IOAPIC, TPM, ...) is still
        // carved out of RAM at the top of 4 GiB. That range must be
        // reported so consumers see the same layout the allocator
        // produced.
        let mut config = input(&[2 * GB], None);
        config.layout.chipset_low_mmio_size = 0;
        config.layout.chipset_high_mmio_size = 0;

        let result = resolve_memory_layout(config).unwrap();

        let low = result.chipset_mmio.low;
        assert_eq!(low.end(), 4 * GB);
        assert!(low.contains(&ARCH_RESERVED));
        assert!(result.chipset_mmio.high.is_empty());
        // The reported ranges must appear in MemoryLayout::mmio() preserving
        // the positional contract: [0] = low, [1] = high (empty placeholder).
        assert_eq!(
            result.memory_layout.mmio(),
            &[low, result.chipset_mmio.high]
        );
    }

    #[test]
    fn asymmetric_chipset_mmio_is_accepted() {
        // Asymmetric chipset MMIO (only low or only high) is allowed.
        // The missing range is EMPTY.
        let mut config = input(&[2 * GB], None);
        config.layout.chipset_high_mmio_size = 0;
        let result = resolve_memory_layout(config).unwrap();
        assert!(!result.chipset_mmio.low.is_empty());
        assert!(result.chipset_mmio.high.is_empty());

        let mut config = input(&[2 * GB], None);
        config.layout.chipset_low_mmio_size = 0;
        let result = resolve_memory_layout(config).unwrap();
        // Low is always at least the arch reserved zone.
        assert!(!result.chipset_mmio.low.is_empty());
        // High is still configured in this case.
        assert!(!result.chipset_mmio.high.is_empty());
    }

    #[test]
    fn fixed_low_mmio_above_4gb_is_rejected() {
        let root_complexes = [pcie_root_complex(
            // A 1 GiB fixed low MMIO range placed above 4 GiB violates the
            // Mmio32 placement contract.
            PcieMmioRangeConfig::Fixed(MemoryRange::new(5 * GB..6 * GB)),
            PcieMmioRangeConfig::Dynamic { size: GB },
        )];
        let mut config = input(&[2 * GB], None);
        config.pcie_root_complexes = &root_complexes;
        let err = resolve_memory_layout(config).unwrap_err();
        assert!(
            err.to_string().contains("must end at or below 4 GiB"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn vtl2_framebuffer_allocation() {
        let framebuffer_size = 8 * MB;
        let mut config = input(&[2 * GB], None);
        config.vtl2_framebuffer_size = framebuffer_size;
        let result = resolve_memory_layout(config).unwrap();

        let gpa_base = result
            .vtl2_framebuffer_gpa_base
            .expect("framebuffer GPA should be allocated");
        // Must be page-aligned.
        assert_eq!(gpa_base % PAGE_SIZE, 0, "framebuffer GPA not page-aligned");
        let fb_range = MemoryRange::new(gpa_base..gpa_base + framebuffer_size);
        // Must not overlap RAM.
        for ram in result.memory_layout.ram() {
            assert!(
                !fb_range.overlaps(&ram.range),
                "framebuffer {fb_range} overlaps RAM {}",
                ram.range,
            );
        }
        // Must not overlap chipset MMIO.
        for mmio in result.memory_layout.mmio() {
            assert!(
                !fb_range.overlaps(mmio),
                "framebuffer {fb_range} overlaps MMIO {mmio}",
            );
        }
    }

    #[test]
    fn vtl2_framebuffer_zero_size_returns_none() {
        let mut config = input(&[2 * GB], None);
        config.vtl2_framebuffer_size = 0;
        let result = resolve_memory_layout(config).unwrap();
        assert!(result.vtl2_framebuffer_gpa_base.is_none());
    }
}
