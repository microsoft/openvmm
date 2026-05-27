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
struct SubtreeRequirement {
    /// Total aligned size needed in the mem32 (non-prefetchable) pool.
    mem32: u64,
    /// Total aligned size needed in the mem64 (prefetchable) pool.
    mem64: u64,
    /// Required alignment for the mem32 pool (max of bridge granularity
    /// and the largest BAR in the subtree).
    align32: u64,
    /// Required alignment for the mem64 pool.
    align64: u64,
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
    devices: &[DiscoveredDevice],
    params: &AssignmentParams,
) -> Result<AssignmentResult, AssignmentError> {
    let mut entries = Vec::new();

    // Step 1: Bottom-up — compute total resource requirements.
    let root_req = compute_subtree_requirement(devices);

    // Step 2: Top-down — allocate from apertures and assign addresses.
    // Align the effective base to the root's alignment requirement so that
    // internal bump allocation matches the sizing (which starts from zero).
    if root_req.mem32 > 0 {
        let aperture =
            params
                .low_mmio
                .or(params.high_mmio)
                .ok_or(AssignmentError::MmioExhaustion {
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

        assign_subtree(devices, false, base, &mut entries);
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

        // If sharing the same aperture as mem32, allocate after them.
        let base = if params.high_mmio.is_some() {
            align_up(aperture.base, root_req.align64)
        } else {
            align_up(aperture.base + root_req.mem32, root_req.align64)
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

        assign_subtree(devices, true, base, &mut entries);
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
fn compute_subtree_requirement(devices: &[DiscoveredDevice]) -> SubtreeRequirement {
    let mut mem32: u64 = 0;
    let mut mem64: u64 = 0;

    // Track the required alignment for each pool. This is the maximum of
    // BRIDGE_WINDOW_ALIGN and the largest BAR (or nested bridge alignment)
    // in the subtree — the parent must place this subtree at an address
    // aligned to this value so that internal bump allocation matches the
    // sizing computed here (which starts from zero, where all alignments
    // are trivially satisfied).
    let mut align32: u64 = BRIDGE_WINDOW_ALIGN;
    let mut align64: u64 = BRIDGE_WINDOW_ALIGN;

    struct Demand {
        size: u64,
        alignment: u64,
        is_mem64: bool,
    }

    let mut demands: Vec<Demand> = Vec::new();

    for dev in devices {
        for bar in &dev.bars {
            demands.push(Demand {
                size: bar.size,
                alignment: bar.size, // BARs are naturally aligned
                is_mem64: is_mem64_bar(bar),
            });
        }

        if dev.is_bridge {
            let child_req = compute_subtree_requirement(&dev.children);
            if child_req.mem32 > 0 {
                let size = align_up(child_req.mem32, BRIDGE_WINDOW_ALIGN);
                demands.push(Demand {
                    size,
                    alignment: child_req.align32,
                    is_mem64: false,
                });
            }
            if child_req.mem64 > 0 {
                let size = align_up(child_req.mem64, BRIDGE_WINDOW_ALIGN);
                demands.push(Demand {
                    size,
                    alignment: child_req.align64,
                    is_mem64: true,
                });
            }
        }
    }

    // Sum with proper alignment, largest-alignment-first within each pool.
    // Placing the most alignment-demanding items first minimizes padding
    // waste, since the offset is most likely well-aligned at the start.
    let mut demands_32: Vec<&Demand> = demands.iter().filter(|d| !d.is_mem64).collect();
    let mut demands_64: Vec<&Demand> = demands.iter().filter(|d| d.is_mem64).collect();
    demands_32.sort_by_key(|d| std::cmp::Reverse(d.alignment));
    demands_64.sort_by_key(|d| std::cmp::Reverse(d.alignment));

    for d in &demands_32 {
        mem32 = align_up(mem32, d.alignment);
        mem32 += d.size;
        align32 = align32.max(d.alignment);
    }
    for d in &demands_64 {
        mem64 = align_up(mem64, d.alignment);
        mem64 += d.size;
        align64 = align64.max(d.alignment);
    }

    SubtreeRequirement {
        mem32,
        mem64,
        align32,
        align64,
    }
}

/// Top-down: assign addresses to devices within a subtree, carving from
/// the given base address. `alloc_64bit` selects which pool (mem32 or
/// mem64) we are assigning.
fn assign_subtree(
    devices: &[DiscoveredDevice],
    alloc_64bit: bool,
    base: u64,
    entries: &mut Vec<AssignmentEntry>,
) {
    // Collect all demands at this level as (size, device_index, kind).
    // Kind distinguishes endpoint BARs from bridge subtree requirements.
    enum Demand {
        Bar {
            dev_idx: usize,
            bar_index: u8,
            is_64bit: bool,
        },
        BridgeSubtree {
            dev_idx: usize,
            alignment: u64,
        },
    }

    let mut demands: Vec<(u64, Demand)> = Vec::new();

    for (i, dev) in devices.iter().enumerate() {
        for bar in &dev.bars {
            if is_mem64_bar(bar) == alloc_64bit {
                demands.push((
                    bar.size,
                    Demand::Bar {
                        dev_idx: i,
                        bar_index: bar.index,
                        is_64bit: bar.is_64bit,
                    },
                ));
            }
        }

        if dev.is_bridge {
            let child_req = compute_subtree_requirement(&dev.children);
            let (size, alignment) = if alloc_64bit {
                (child_req.mem64, child_req.align64)
            } else {
                (child_req.mem32, child_req.align32)
            };
            if size > 0 {
                let aligned = align_up(size, BRIDGE_WINDOW_ALIGN);
                demands.push((
                    aligned,
                    Demand::BridgeSubtree {
                        dev_idx: i,
                        alignment,
                    },
                ));
            }
        }
    }

    // Sort by alignment descending. This must match the order used in
    // compute_subtree_requirement so that the sizing and assignment
    // produce consistent results.
    demands.sort_by_key(|d| {
        let alignment = match &d.1 {
            Demand::Bar { .. } => d.0,
            Demand::BridgeSubtree { alignment, .. } => *alignment,
        };
        std::cmp::Reverse(alignment)
    });

    // Bump-allocate.
    let mut offset = base;
    for (size, demand) in &demands {
        // BARs are naturally aligned to their size (always a power of two).
        // Bridge subtrees are aligned to the max of bridge window
        // granularity and the largest internal alignment requirement.
        let alignment = match demand {
            Demand::Bar { .. } => *size,
            Demand::BridgeSubtree { alignment, .. } => *alignment,
        };
        offset = align_up(offset, alignment);

        match demand {
            Demand::Bar {
                dev_idx,
                bar_index,
                is_64bit,
            } => {
                let dev = &devices[*dev_idx];
                let entry = find_or_create_entry(entries, dev.bus, dev.device, dev.function);
                entry.bars.push(BarAssignment {
                    index: *bar_index,
                    address: offset,
                    size: *size,
                    is_64bit: *is_64bit,
                });
            }
            Demand::BridgeSubtree { dev_idx, .. } => {
                let dev = &devices[*dev_idx];
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
                assign_subtree(&dev.children, alloc_64bit, offset, entries);
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
            cfg.write_u32(
                entry.bus,
                entry.device,
                entry.function,
                offset,
                bar.address as u32,
            )
            .await;

            if bar.is_64bit {
                let upper_offset = HeaderType00::BAR0.0 + ((bar.index + 1) as u16) * 4;
                cfg.write_u32(
                    entry.bus,
                    entry.device,
                    entry.function,
                    upper_offset,
                    (bar.address >> 32) as u32,
                )
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
            cfg.write_u32(
                entry.bus,
                entry.device,
                entry.function,
                HeaderType01::SEC_STATUS_IO_RANGE.0,
                0x0000_00F0, // base 0xF0 > limit 0x00, status bits zeroed
            )
            .await;

            // Non-prefetchable memory window (32-bit only).
            let value = if let (Some(base), Some(limit)) = (entry.memory_base, entry.memory_limit) {
                let mem_base_reg = ((base >> 16) as u16 & MEMORY_BASE_LIMIT_ADDRESS_MASK) as u32;
                let mem_limit_reg = ((limit >> 16) as u16 & MEMORY_BASE_LIMIT_ADDRESS_MASK) as u32;
                mem_base_reg | (mem_limit_reg << 16)
            } else {
                0x0000_fff0 // base 0xFFF0 > limit 0x0000
            };
            cfg.write_u32(
                entry.bus,
                entry.device,
                entry.function,
                HeaderType01::MEMORY_RANGE.0,
                value,
            )
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
            cfg.write_u32(
                entry.bus,
                entry.device,
                entry.function,
                HeaderType01::PREFETCH_LIMIT_UPPER.0,
                pf_limit_upper,
            )
            .await;
            cfg.write_u32(
                entry.bus,
                entry.device,
                entry.function,
                HeaderType01::PREFETCH_RANGE.0,
                pf_range,
            )
            .await;
            cfg.write_u32(
                entry.bus,
                entry.device,
                entry.function,
                HeaderType01::PREFETCH_BASE_UPPER.0,
                pf_base_upper,
            )
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
