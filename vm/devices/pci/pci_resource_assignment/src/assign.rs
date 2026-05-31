// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Phase 2: Bottom-up aperture computation and top-down address assignment.

use crate::AssignmentEntry;
use crate::AssignmentError;
use crate::AssignmentParams;
use crate::AssignmentResult;
use crate::BarAssignment;
use crate::PciConfigAccess;
use crate::enumerate::DiscoveredDevice;
use pci_core::spec::cfg_space::HeaderType00;
use pci_core::spec::cfg_space::HeaderType01;
use pci_core::spec::cfg_space::MEMORY_BASE_LIMIT_ADDRESS_MASK;

/// Bridge memory window granularity: 1 MB.
const BRIDGE_WINDOW_ALIGN: u64 = 1 << 20;

fn find_or_create_entry(
    entries: &mut Vec<AssignmentEntry>,
    bus: u8,
    device: u8,
    function: u8,
) -> &mut AssignmentEntry {
    let pos = entries
        .iter()
        .position(|e| e.bus == bus && e.device == device && e.function == function);
    if let Some(idx) = pos {
        &mut entries[idx]
    } else {
        entries.push(AssignmentEntry {
            bus,
            device,
            function,
            bars: Vec::new(),
            secondary_bus: None,
            subordinate_bus: None,
            memory_base: None,
            memory_limit: None,
            prefetchable_base: None,
            prefetchable_limit: None,
        });
        entries.last_mut().unwrap()
    }
}

/// Resource requirement for a subtree (bridge or root).
#[derive(Debug, Clone)]
pub(crate) struct SubtreeRequirement {
    /// Total aligned size needed in the mem32 (non-prefetchable) pool.
    mem32: u64,
    /// Total aligned size needed in the mem64 (prefetchable) pool.
    mem64: u64,
    /// Required alignment for the mem32 pool (max of bridge granularity
    /// and the largest BAR in the subtree).
    align32: u64,
    /// Required alignment for the mem64 pool.
    align64: u64,
    /// Sorted demands for this level's devices, used by the assignment
    /// pass to avoid recomputing them.
    demands: Vec<Demand>,
}

/// A single resource demand at one level of the PCI tree.
///
/// Shared between the sizing pass (which sums sizes) and the assignment
/// pass (which bump-allocates addresses).
#[derive(Debug, Clone)]
enum Demand {
    /// An endpoint BAR.
    Bar {
        dev_idx: usize,
        bar_index: u8,
        size: u64,
        is_64bit: bool,
        is_mem64: bool,
    },
    /// A bridge's child subtree window.
    BridgeSubtree {
        dev_idx: usize,
        /// Aligned size of the bridge window.
        size: u64,
        alignment: u64,
        is_mem64: bool,
    },
    /// VF BAR space — reserved for SR-IOV VFs.
    SriovVfBars {
        /// Total size (per-VF BAR size * total_vfs).
        size: u64,
        /// Per-VF BAR size (alignment requirement).
        alignment: u64,
        is_mem64: bool,
    },
}

impl Demand {
    fn size(&self) -> u64 {
        match self {
            Demand::Bar { size, .. }
            | Demand::BridgeSubtree { size, .. }
            | Demand::SriovVfBars { size, .. } => *size,
        }
    }

    fn alignment(&self) -> u64 {
        match self {
            Demand::Bar { size, .. } => *size, // BARs are naturally aligned
            Demand::BridgeSubtree { alignment, .. } | Demand::SriovVfBars { alignment, .. } => {
                *alignment
            }
        }
    }

    fn is_mem64(&self) -> bool {
        match self {
            Demand::Bar { is_mem64, .. }
            | Demand::BridgeSubtree { is_mem64, .. }
            | Demand::SriovVfBars { is_mem64, .. } => *is_mem64,
        }
    }
}

/// Assign addresses to all discovered devices.
///
/// Uses hierarchical bottom-up/top-down allocation:
///
/// 1. **Bottom-up sizing**: Each bridge computes the total aligned
///    resource requirement of its subtree.
/// 2. **Top-down assignment**: The host bridge carves its aperture among
///    top-level devices/bridges. Each bridge subdivides its allocated
///    range among children, largest-first.
///
/// BARs are split into two pools:
///
/// - **mem32 (low MMIO):** all non-prefetchable BARs and 32-bit
///   prefetchable BARs. These use the non-prefetchable bridge window.
///
/// - **mem64 (high MMIO):** 64-bit prefetchable BARs only. These use
///   the prefetchable bridge window.
///
/// Returns an error if any BAR cannot be placed.
pub fn assign_addresses(
    devices: &mut [DiscoveredDevice],
    params: &AssignmentParams,
) -> Result<AssignmentResult, AssignmentError> {
    let mut entries = Vec::new();

    // Step 1: Bottom-up — compute total resource requirements.
    // This also stores per-bridge requirements on the DiscoveredDevice
    // nodes so that the assignment pass can read them without recomputing.
    let root_req = compute_subtree_requirement(devices);

    // Step 2: Top-down — allocate from apertures and assign addresses.
    // Align the effective base to the root's alignment requirement so that
    // internal bump allocation matches the sizing (which starts from zero).
    // Track where mem32 ends so that mem64 can start after it when
    // sharing the same aperture.
    let mut mem32_end: Option<u64> = None;

    if root_req.mem32 > 0 {
        // 32-bit BARs and non-prefetchable bridge windows are inherently
        // 32-bit, so the aperture must be below 4 GB. Do not fall back
        // to high_mmio — placing 32-bit BARs above 4 GB would silently
        // truncate addresses.
        let aperture = params.low_mmio.ok_or(AssignmentError::MmioExhaustion {
            required: root_req.mem32,
            available: 0,
            aperture: "low_mmio",
        })?;

        let base = align_up(aperture.base, root_req.align32);
        let aperture_end = aperture.base + aperture.len;
        let available = aperture_end.saturating_sub(base);
        if root_req.mem32 > available {
            return Err(AssignmentError::MmioExhaustion {
                required: root_req.mem32,
                available,
                aperture: "low_mmio",
            });
        }

        assign_subtree(devices, &root_req.demands, false, base, &mut entries);

        mem32_end = Some(base + root_req.mem32);
    }

    if root_req.mem64 > 0 {
        let aperture =
            params
                .high_mmio
                .or(params.low_mmio)
                .ok_or(AssignmentError::MmioExhaustion {
                    required: root_req.mem64,
                    available: 0,
                    aperture: "high_mmio",
                })?;

        // If sharing the same aperture as mem32, allocate after the
        // actual aligned mem32 end to avoid overlapping assignments.
        let base = if let Some(end) = mem32_end.filter(|_| params.high_mmio.is_none()) {
            align_up(end, root_req.align64)
        } else {
            align_up(aperture.base, root_req.align64)
        };

        let aperture_end = aperture.base + aperture.len;
        let available = aperture_end.saturating_sub(base);
        if root_req.mem64 > available {
            return Err(AssignmentError::MmioExhaustion {
                required: root_req.mem64,
                available,
                aperture: "high_mmio",
            });
        }

        assign_subtree(devices, &root_req.demands, true, base, &mut entries);
    }

    let result = AssignmentResult { entries };
    validate_assignments(&result, params);
    Ok(result)
}

/// Verify that all assigned BAR addresses fall within the provided apertures.
fn validate_assignments(result: &AssignmentResult, params: &AssignmentParams) {
    for entry in &result.entries {
        for bar in &entry.bars {
            let bar_end = bar.address + bar.size;
            let in_low = params
                .low_mmio
                .is_some_and(|a| bar.address >= a.base && bar_end <= a.base + a.len);
            let in_high = params
                .high_mmio
                .is_some_and(|a| bar.address >= a.base && bar_end <= a.base + a.len);
            assert!(
                in_low || in_high,
                "BAR {bus:02x}:{dev:02x}.{func} index {idx} at {addr:#x}..{end:#x} \
                 is outside all MMIO apertures",
                bus = entry.bus,
                dev = entry.device,
                func = entry.function,
                idx = bar.index,
                addr = bar.address,
                end = bar_end,
            );
        }
    }
}
fn is_mem64_bar(bar: &crate::enumerate::DiscoveredBar) -> bool {
    bar.is_64bit && bar.is_prefetchable
}

/// Bottom-up: compute the total aligned resource requirement for a list
/// of devices (which may be the root level or children behind a bridge).
///
/// Also builds and stores the sorted demand list so that `assign_subtree`
/// can reuse it without recomputing.
fn compute_subtree_requirement(devices: &mut [DiscoveredDevice]) -> SubtreeRequirement {
    let mut demands: Vec<Demand> = Vec::new();

    for (i, dev) in devices.iter_mut().enumerate() {
        for bar in &dev.bars {
            demands.push(Demand::Bar {
                dev_idx: i,
                bar_index: bar.index,
                size: bar.size,
                is_64bit: bar.is_64bit,
                is_mem64: is_mem64_bar(bar),
            });
        }

        // SR-IOV PF: account for VF BAR space (TotalVFs * per-VF BAR size).
        if let Some(sriov) = &dev.sriov {
            for bar in &sriov.vf_bars {
                let total_size = bar.size.saturating_mul(sriov.total_vfs as u64);
                demands.push(Demand::SriovVfBars {
                    size: total_size,
                    // VF BAR region base must be aligned to per-VF BAR size
                    // (each VF's BAR is at base + n * bar_size).
                    alignment: bar.size,
                    is_mem64: is_mem64_bar(bar),
                });
            }
        }

        if dev.is_bridge {
            let child_req = compute_subtree_requirement(&mut dev.children);
            if child_req.mem32 > 0 {
                let size = align_up(child_req.mem32, BRIDGE_WINDOW_ALIGN);
                demands.push(Demand::BridgeSubtree {
                    dev_idx: i,
                    size,
                    alignment: child_req.align32,
                    is_mem64: false,
                });
            }
            if child_req.mem64 > 0 {
                let size = align_up(child_req.mem64, BRIDGE_WINDOW_ALIGN);
                demands.push(Demand::BridgeSubtree {
                    dev_idx: i,
                    size,
                    alignment: child_req.align64,
                    is_mem64: true,
                });
            }
            dev.subtree_req = Some(child_req);
        }
    }

    // Sum with proper alignment, largest-alignment-first. Placing the most
    // alignment-demanding items first minimizes padding waste.
    demands.sort_by_key(|d| std::cmp::Reverse(d.alignment()));

    let mut mem32: u64 = 0;
    let mut mem64: u64 = 0;
    let mut align32 = BRIDGE_WINDOW_ALIGN;
    let mut align64 = BRIDGE_WINDOW_ALIGN;

    for d in &demands {
        if d.is_mem64() {
            mem64 = align_up(mem64, d.alignment());
            mem64 += d.size();
            align64 = align64.max(d.alignment());
        } else {
            mem32 = align_up(mem32, d.alignment());
            mem32 += d.size();
            align32 = align32.max(d.alignment());
        }
    }

    SubtreeRequirement {
        mem32,
        mem64,
        align32,
        align64,
        demands,
    }
}

/// Top-down: assign addresses to devices within a subtree, carving from
/// the given base address. `alloc_64bit` selects which pool (mem32 or
/// mem64) we are assigning.
///
/// `demands` is the pre-sorted demand list built by `compute_subtree_requirement`.
fn assign_subtree(
    devices: &mut [DiscoveredDevice],
    demands: &[Demand],
    alloc_64bit: bool,
    base: u64,
    entries: &mut Vec<AssignmentEntry>,
) {
    // Bump-allocate from the pre-sorted demands, skipping demands that
    // belong to the other pool.
    let mut offset = base;
    for demand in demands {
        if demand.is_mem64() != alloc_64bit {
            continue;
        }

        let size = demand.size();
        offset = align_up(offset, demand.alignment());

        match *demand {
            Demand::Bar {
                dev_idx,
                bar_index,
                size,
                is_64bit,
                ..
            } => {
                let dev = &devices[dev_idx];
                let entry = find_or_create_entry(entries, dev.bus, dev.device, dev.function);
                entry.bars.push(BarAssignment {
                    index: bar_index,
                    address: offset,
                    size,
                    is_64bit,
                });
            }
            Demand::BridgeSubtree { dev_idx, size, .. } => {
                let dev = &mut devices[dev_idx];
                let entry = find_or_create_entry(entries, dev.bus, dev.device, dev.function);
                entry.secondary_bus = dev.secondary_bus;
                entry.subordinate_bus = dev.subordinate_bus;

                // Set the bridge window to the carved-out range.
                let window_base = offset;
                let window_limit = offset + size - 1;
                if alloc_64bit {
                    entry.prefetchable_base = Some(window_base);
                    entry.prefetchable_limit = Some(window_limit);
                } else {
                    entry.memory_base = Some(window_base);
                    entry.memory_limit = Some(window_limit);
                }

                // Recurse into children with this bridge's carved-out range.
                let child_demands = &dev
                    .subtree_req
                    .as_ref()
                    .expect("subtree_req must be populated by compute_subtree_requirement")
                    .demands;
                assign_subtree(
                    &mut dev.children,
                    child_demands,
                    alloc_64bit,
                    offset,
                    entries,
                );
            }
            Demand::SriovVfBars { .. } => {
                // VF BAR space is reserved but not explicitly assigned.
                // The guest programs VF BAR addresses through the SR-IOV
                // capability registers.
            }
        }

        offset += size;
    }

    // Ensure bridge entries exist even if they have no BARs in this pool
    // (needed for bridges that only have resources in the other pool).
    for dev in devices {
        if dev.is_bridge {
            let entry = find_or_create_entry(entries, dev.bus, dev.device, dev.function);
            entry.secondary_bus = dev.secondary_bus;
            entry.subordinate_bus = dev.subordinate_bus;
        }
    }
}

/// Program all assignments into config space.
///
/// Writes BAR addresses and bridge memory windows for every entry in
/// `result`. This function assumes MMIO decode (MSE) has already been
/// cleared by the enumeration phase and does not modify the command
/// register.
pub async fn program_assignments(cfg: &mut impl PciConfigAccess, result: &AssignmentResult) {
    for entry in &result.entries {
        // Program BAR addresses.
        for bar in &entry.bars {
            let offset = HeaderType00::BAR0.0 + (bar.index as u16) * 4;
            entry.write_cfg(cfg, offset, bar.address as u32).await;

            if bar.is_64bit {
                let upper_offset = HeaderType00::BAR0.0 + ((bar.index + 1) as u16) * 4;
                entry
                    .write_cfg(cfg, upper_offset, (bar.address >> 32) as u32)
                    .await;
            }
        }

        // Program bridge windows. For bridges, explicitly disable any unused
        // window by writing base > limit so that the guest OS's probe
        // (write-readback) doesn't mistake zeroed registers for a valid window.
        if entry.secondary_bus.is_some() {
            // I/O window — we don't assign I/O BARs, so always disable.
            // Write zeros to the upper 16 bits (Secondary Status) since
            // those bits are W1C — writing back a read value would clear them.
            entry
                .write_cfg(cfg, HeaderType01::SEC_STATUS_IO_RANGE.0, 0x0000_00F0)
                .await;

            // Non-prefetchable memory window (32-bit only).
            let value = if let (Some(base), Some(limit)) = (entry.memory_base, entry.memory_limit) {
                let mem_base_reg = ((base >> 16) as u16 & MEMORY_BASE_LIMIT_ADDRESS_MASK) as u32;
                let mem_limit_reg = ((limit >> 16) as u16 & MEMORY_BASE_LIMIT_ADDRESS_MASK) as u32;
                mem_base_reg | (mem_limit_reg << 16)
            } else {
                0x0000_fff0
            };
            entry
                .write_cfg(cfg, HeaderType01::MEMORY_RANGE.0, value)
                .await;

            // Prefetchable memory window (64-bit capable).
            // Use base > limit to disable when no window is assigned.
            let (pf_range, pf_base_upper, pf_limit_upper) = if let (Some(base), Some(limit)) =
                (entry.prefetchable_base, entry.prefetchable_limit)
            {
                let pf_base_reg =
                    ((base >> 16) as u16 & MEMORY_BASE_LIMIT_ADDRESS_MASK) as u32 | 0x1;
                let pf_limit_reg =
                    ((limit >> 16) as u16 & MEMORY_BASE_LIMIT_ADDRESS_MASK) as u32 | 0x1;
                (
                    pf_base_reg | (pf_limit_reg << 16),
                    (base >> 32) as u32,
                    (limit >> 32) as u32,
                )
            } else {
                (0x0000_fff0, 0xFFFF_FFFF, 0)
            };
            entry
                .write_cfg(cfg, HeaderType01::PREFETCH_LIMIT_UPPER.0, pf_limit_upper)
                .await;
            entry
                .write_cfg(cfg, HeaderType01::PREFETCH_RANGE.0, pf_range)
                .await;
            entry
                .write_cfg(cfg, HeaderType01::PREFETCH_BASE_UPPER.0, pf_base_upper)
                .await;
        }

        if entry.bars.is_empty() {
            tracing::debug!(
                bus = entry.bus,
                device = entry.device,
                function = entry.function,
                ?entry.secondary_bus,
                ?entry.subordinate_bus,
                ?entry.memory_base,
                ?entry.memory_limit,
                ?entry.prefetchable_base,
                ?entry.prefetchable_limit,
                "bridge programmed"
            );
        } else {
            for bar in &entry.bars {
                tracing::debug!(
                    bus = entry.bus,
                    device = entry.device,
                    function = entry.function,
                    bar_index = bar.index,
                    address = format_args!("{:#x}", bar.address),
                    size = format_args!("{:#x}", bar.size),
                    is_64bit = bar.is_64bit,
                    "BAR programmed"
                );
            }
        }
    }
}

fn align_up(value: u64, alignment: u64) -> u64 {
    assert!(alignment.is_power_of_two());
    (value + alignment - 1) & !(alignment - 1)
}
