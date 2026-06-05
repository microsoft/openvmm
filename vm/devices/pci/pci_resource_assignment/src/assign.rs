// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Phase 2: Bottom-up aperture computation and top-down address assignment.

use crate::AssignmentError;
use crate::AssignmentParams;
use crate::PciConfigAccess;
use crate::enumerate::DiscoveredDevice;
use pci_core::spec::caps::sriov::SriovExtendedCapabilityHeader;
use pci_core::spec::cfg_space::HeaderType00;
use pci_core::spec::cfg_space::HeaderType01;
use pci_core::spec::cfg_space::MEMORY_BASE_LIMIT_ADDRESS_MASK;

/// Bridge memory window granularity: 1 MB.
const BRIDGE_WINDOW_ALIGN: u64 = 1 << 20;

/// Resource requirement for a subtree (bridge or root).
#[derive(Debug, Clone)]
pub(crate) struct SubtreeState {
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
    /// Assigned non-prefetchable bridge window (base, limit). Set by assign_subtree.
    memory_window: Option<(u64, u64)>,
    /// Assigned prefetchable bridge window (base, limit). Set by assign_subtree.
    prefetchable_window: Option<(u64, u64)>,
    /// If pinned demands exist in the mem32 pool, the required base address
    /// for this subtree's window (align_down of the lowest pinned address).
    constrained_base32: Option<u64>,
    /// If pinned demands exist in the mem64 pool, the required base address
    /// for this subtree's window.
    constrained_base64: Option<u64>,
    /// Pre-computed free gaps in the mem32 pool, covering the window
    /// [constrained_base, constrained_base + align_up(mem, 1 MB)).
    /// Empty when there are no pinned demands.
    gaps32: Vec<(u64, u64)>,
    /// Pre-computed free gaps in the mem64 pool.
    gaps64: Vec<(u64, u64)>,
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
        is_mem64: bool,
        /// If set, this BAR is pinned to a specific address (pre-programmed
        /// in config space, discovered via `preserve_bars`).
        pinned_address: Option<u64>,
    },
    /// A bridge's child subtree window.
    BridgeSubtree {
        dev_idx: usize,
        /// Aligned size of the bridge window.
        size: u64,
        alignment: u64,
        is_mem64: bool,
        /// If set, this bridge has pinned descendants and must be placed
        /// at this specific base address.
        constrained_base: Option<u64>,
    },
    /// VF BAR space — reserved for SR-IOV VFs.
    SriovVfBars {
        /// Index of the device in the parent's device list.
        dev_idx: usize,
        /// VF BAR register index within the SR-IOV capability.
        bar_index: u8,
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

    /// Returns `Some((address, size))` if this demand has a fixed position
    /// (pinned BAR or constrained bridge).
    fn fixed_position(&self) -> Option<(u64, u64)> {
        match self {
            Demand::Bar {
                pinned_address: Some(addr),
                size,
                ..
            } => Some((*addr, *size)),
            Demand::BridgeSubtree {
                constrained_base: Some(base),
                size,
                ..
            } => Some((*base, *size)),
            _ => None,
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
) -> Result<(), AssignmentError> {
    // Step 1: Bottom-up — compute total resource requirements.
    // This also stores per-bridge requirements on the DiscoveredDevice
    // nodes so that the assignment pass can read them without recomputing.
    let mut root_req = compute_subtree_state(devices);

    // Validate pinned BAR constraints before assigning addresses.
    validate_pinned_bars(devices, params)?;

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

        // If there are constrained (pinned) demands, use their base.
        // Otherwise, align to the aperture base as before.
        let base = if let Some(cbase) = root_req.constrained_base32 {
            cbase
        } else {
            align_up(aperture.base, root_req.align32)
        };
        let aperture_end = aperture.base + aperture.len;
        let available = aperture_end.saturating_sub(base);
        if root_req.mem32 > available {
            return Err(AssignmentError::MmioExhaustion {
                required: root_req.mem32,
                available,
                aperture: "low_mmio",
            });
        }

        if root_req.constrained_base32.is_none() {
            root_req.gaps32.push((base, aperture_end));
        }
        assign_subtree(devices, &root_req.demands, &mut root_req.gaps32, false);

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

        // If there are constrained (pinned) demands, use their base.
        let base = if let Some(cbase) = root_req.constrained_base64 {
            cbase
        } else if let Some(end) = mem32_end.filter(|_| params.high_mmio.is_none()) {
            // If sharing the same aperture as mem32, allocate after the
            // actual aligned mem32 end to avoid overlapping assignments.
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

        if root_req.constrained_base64.is_none() {
            root_req.gaps64.push((base, aperture_end));
        }
        assign_subtree(devices, &root_req.demands, &mut root_req.gaps64, true);
    }

    validate_assignments(devices, params);
    Ok(())
}

/// Verify that all assigned BAR addresses fall within the provided apertures.
fn validate_assignments(devices: &[DiscoveredDevice], params: &AssignmentParams) {
    for dev in devices {
        for bar in &dev.bars {
            let address = bar.address.unwrap();
            assert_bar_in_aperture(address, bar.size, dev, bar.index, params);
        }
        if let Some(sriov) = &dev.sriov {
            for bar in &sriov.vf_bars {
                let address = bar.address.unwrap();
                let total_size = bar.size * sriov.total_vfs as u64;
                assert_bar_in_aperture(address, total_size, dev, bar.index, params);
            }
        }
        // Validate bridge windows fit within their respective apertures
        // and that child windows fit within parent windows.
        if let Some(req) = &dev.subtree_req {
            if let Some((base, limit)) = req.memory_window {
                let size = limit - base + 1;
                assert!(
                    params
                        .low_mmio
                        .is_some_and(|a| base >= a.base && base + size <= a.base + a.len),
                    "bridge {bus:02x}:{device:02x}.{func} memory window \
                     {base:#x}..={limit:#x} exceeds low_mmio aperture",
                    bus = dev.bus,
                    device = dev.device,
                    func = dev.function,
                );
            }
            if let Some((base, limit)) = req.prefetchable_window {
                let size = limit - base + 1;
                let in_low = params
                    .low_mmio
                    .is_some_and(|a| base >= a.base && base + size <= a.base + a.len);
                let in_high = params
                    .high_mmio
                    .is_some_and(|a| base >= a.base && base + size <= a.base + a.len);
                assert!(
                    in_low || in_high,
                    "bridge {bus:02x}:{device:02x}.{func} prefetchable window \
                     {base:#x}..={limit:#x} exceeds MMIO apertures",
                    bus = dev.bus,
                    device = dev.device,
                    func = dev.function,
                );
            }
            // Check child bridge windows are contained within this bridge's windows.
            for child in &dev.children {
                if let Some(child_req) = &child.subtree_req {
                    if let (Some((cb, cl)), Some((pb, pl))) =
                        (child_req.memory_window, req.memory_window)
                    {
                        assert!(
                            cb >= pb && cl <= pl,
                            "child bridge {cbus:02x}:{cdev:02x}.{cfunc} memory window \
                             {cb:#x}..={cl:#x} exceeds parent {pb:#x}..={pl:#x}",
                            cbus = child.bus,
                            cdev = child.device,
                            cfunc = child.function,
                        );
                    }
                    if let (Some((cb, cl)), Some((pb, pl))) =
                        (child_req.prefetchable_window, req.prefetchable_window)
                    {
                        assert!(
                            cb >= pb && cl <= pl,
                            "child bridge {cbus:02x}:{cdev:02x}.{cfunc} prefetchable window \
                             {cb:#x}..={cl:#x} exceeds parent {pb:#x}..={pl:#x}",
                            cbus = child.bus,
                            cdev = child.device,
                            cfunc = child.function,
                        );
                    }
                }
            }
        }
        validate_assignments(&dev.children, params);
    }
}

fn assert_bar_in_aperture(
    address: u64,
    size: u64,
    dev: &DiscoveredDevice,
    index: u8,
    params: &AssignmentParams,
) {
    let bar_end = address + size;
    let in_low = params
        .low_mmio
        .is_some_and(|a| address >= a.base && bar_end <= a.base + a.len);
    let in_high = params
        .high_mmio
        .is_some_and(|a| address >= a.base && bar_end <= a.base + a.len);
    assert!(
        in_low || in_high,
        "BAR {bus:02x}:{device:02x}.{func} index {idx} at {addr:#x}..{end:#x} \
         is outside all MMIO apertures",
        bus = dev.bus,
        device = dev.device,
        func = dev.function,
        idx = index,
        addr = address,
        end = bar_end,
    );
}
fn is_mem64_bar(bar: &crate::enumerate::DiscoveredBar) -> bool {
    bar.is_64bit && bar.is_prefetchable
}

/// Bottom-up: compute the total aligned resource requirement for a list
/// of devices (which may be the root level or children behind a bridge).
///
/// Also builds and stores the sorted demand list so that `assign_subtree`
/// can reuse it without recomputing.
fn compute_subtree_state(devices: &mut [DiscoveredDevice]) -> SubtreeState {
    let mut demands: Vec<Demand> = Vec::new();

    for (i, dev) in devices.iter_mut().enumerate() {
        for bar in &dev.bars {
            // For pinned BARs below 4 GB, treat them as mem32 regardless
            // of encoding. Prefetchable vs. non-prefetchable is not a real
            // distinction in virtualization, and placing a pinned BAR in the
            // mem64 pool when its address is in the low MMIO range would
            // create a bridge window spanning across apertures.
            let is_mem64 = if let Some(addr) = bar.pinned_address {
                addr >= 0x1_0000_0000 && is_mem64_bar(bar)
            } else {
                is_mem64_bar(bar)
            };
            demands.push(Demand::Bar {
                dev_idx: i,
                bar_index: bar.index,
                size: bar.size,
                is_mem64,
                pinned_address: bar.pinned_address,
            });
        }

        // SR-IOV PF: account for VF BAR space (TotalVFs * per-VF BAR size).
        if let Some(sriov) = &dev.sriov {
            for bar in &sriov.vf_bars {
                let total_size = bar.size.saturating_mul(sriov.total_vfs as u64);
                demands.push(Demand::SriovVfBars {
                    dev_idx: i,
                    bar_index: bar.index,
                    size: total_size,
                    // VF BAR region base must be aligned to per-VF BAR size
                    // (each VF's BAR is at base + n * bar_size).
                    alignment: bar.size,
                    is_mem64: is_mem64_bar(bar),
                });
            }
        }

        if dev.is_bridge {
            let child_req = compute_subtree_state(&mut dev.children);
            if child_req.mem32 > 0 {
                let size = align_up(child_req.mem32, BRIDGE_WINDOW_ALIGN);
                demands.push(Demand::BridgeSubtree {
                    dev_idx: i,
                    size,
                    alignment: child_req.align32,
                    is_mem64: false,
                    constrained_base: child_req.constrained_base32,
                });
            }
            if child_req.mem64 > 0 {
                let size = align_up(child_req.mem64, BRIDGE_WINDOW_ALIGN);
                demands.push(Demand::BridgeSubtree {
                    dev_idx: i,
                    size,
                    alignment: child_req.align64,
                    is_mem64: true,
                    constrained_base: child_req.constrained_base64,
                });
            }
            dev.subtree_req = Some(child_req);
        }
    }

    // Sort dynamic demands by alignment descending. Placing the most
    // alignment-demanding items first minimizes padding waste.
    demands.sort_by_key(|d| std::cmp::Reverse(d.alignment()));

    // Collect fixed-position demands per pool.
    let mut pin32: Vec<(u64, u64)> = Vec::new();
    let mut pin64: Vec<(u64, u64)> = Vec::new();
    for d in &demands {
        if let Some((addr, size)) = d.fixed_position() {
            if d.is_mem64() {
                pin64.push((addr, size));
            } else {
                pin32.push((addr, size));
            }
        }
    }

    // Size each pool: compute the pinned span, build gaps, trial-allocate
    // dynamic demands, and extend the window for anything that didn't fit.
    let mut pool32 = size_pool(&mut pin32, &demands, false);
    let mut pool64 = size_pool(&mut pin64, &demands, true);

    // Trial-allocate dynamic demands into gap clones to determine which
    // fit in the pinned span and which extend the window.
    let mut sizing_gaps32 = pool32.gaps.clone();
    let mut sizing_gaps64 = pool64.gaps.clone();
    for d in &demands {
        if d.fixed_position().is_some() {
            continue;
        }
        if d.is_mem64() {
            pool64.align = pool64.align.max(d.alignment());
            if allocate_from_gaps(&mut sizing_gaps64, d.size(), d.alignment()).is_none() {
                pool64.mem = align_up(pool64.mem, d.alignment());
                pool64.mem += d.size();
            }
        } else {
            pool32.align = pool32.align.max(d.alignment());
            if allocate_from_gaps(&mut sizing_gaps32, d.size(), d.alignment()).is_none() {
                pool32.mem = align_up(pool32.mem, d.alignment());
                pool32.mem += d.size();
            }
        }
    }

    // Extend gap lists with the tail region for demands that didn't fit.
    pool32.extend_tail();
    pool64.extend_tail();

    SubtreeState {
        mem32: pool32.mem,
        mem64: pool64.mem,
        align32: pool32.align,
        align64: pool64.align,
        demands,
        memory_window: None,
        prefetchable_window: None,
        constrained_base32: pool32.constrained_base,
        constrained_base64: pool64.constrained_base,
        gaps32: pool32.gaps,
        gaps64: pool64.gaps,
    }
}

/// Per-pool sizing state produced by `size_pool`.
struct PoolState {
    /// Total window size needed (pinned span + dynamic overflow).
    mem: u64,
    /// Required alignment (max of bridge granularity and largest demand).
    align: u64,
    /// If pinned demands exist, the required window base address.
    constrained_base: Option<u64>,
    /// Pre-computed gap list covering the pinned span, for reuse by
    /// `assign_subtree`. Empty when there are no pinned demands.
    gaps: Vec<(u64, u64)>,
    /// End of the pinned span, for extending gaps after sizing.
    pinned_span_end: Option<u64>,
}

impl PoolState {
    /// Append a tail gap for dynamic demands that didn't fit in the
    /// pinned-span gaps. Called after the sizing trial.
    fn extend_tail(&mut self) {
        if let Some(pin_end) = self.pinned_span_end {
            let window_end = self.constrained_base.unwrap() + self.mem;
            if pin_end < window_end {
                self.gaps.push((pin_end, window_end));
            }
        }
    }
}

/// Compute the pinned span, gap list, and initial window size for one
/// pool (mem32 or mem64). `pins` is sorted in place.
fn size_pool(pins: &mut [(u64, u64)], demands: &[Demand], is_mem64: bool) -> PoolState {
    pins.sort_by_key(|&(a, _)| a);

    let (mem, constrained_base) = if !pins.is_empty() {
        let min_addr = pins.iter().map(|(a, _)| *a).min().unwrap();
        let max_end = pins.iter().map(|(a, s)| a + s).max().unwrap();
        let base = align_down(min_addr, BRIDGE_WINDOW_ALIGN);
        (max_end - base, Some(base))
    } else {
        (0, None)
    };

    let _ = (demands, is_mem64); // used by caller for trial allocation

    let gaps = if let Some(base) = constrained_base {
        build_gap_list(base, base + mem, pins)
    } else {
        Vec::new()
    };

    let pinned_span_end = constrained_base.map(|b| b + mem);

    PoolState {
        mem,
        align: BRIDGE_WINDOW_ALIGN,
        constrained_base,
        gaps,
        pinned_span_end,
    }
}

/// Top-down: assign addresses to devices within a subtree, carving from
/// the given gap list. `alloc_64bit` selects which pool (mem32 or
/// mem64) we are assigning.
///
/// Fixed-position demands (pinned BARs and constrained bridges) are placed
/// at their predetermined addresses. Dynamic demands are placed into free
/// gaps using a first-fit strategy.
///
/// `demands` is the pre-sorted demand list built by `compute_subtree_state`.
fn assign_subtree(
    devices: &mut [DiscoveredDevice],
    demands: &[Demand],
    gaps: &mut Vec<(u64, u64)>,
    alloc_64bit: bool,
) {
    for demand in demands {
        if demand.is_mem64() != alloc_64bit {
            continue;
        }

        // Fixed-position demands use their predetermined address.
        // Dynamic demands are placed via the gap allocator.
        let assign_addr = if let Some((addr, _)) = demand.fixed_position() {
            addr
        } else {
            allocate_from_gaps(gaps, demand.size(), demand.alignment())
                .expect("demand must fit (sizing pass guarantees sufficient space)")
        };

        match *demand {
            Demand::Bar {
                dev_idx, bar_index, ..
            } => {
                let bar = devices[dev_idx]
                    .bars
                    .iter_mut()
                    .find(|b| b.index == bar_index)
                    .expect("demand references a BAR that exists");
                bar.address = Some(assign_addr);
            }
            Demand::BridgeSubtree { dev_idx, size, .. } => {
                let dev = &mut devices[dev_idx];

                // Set the bridge window to the carved-out range.
                let window_base = assign_addr;
                let window_limit = assign_addr + size - 1;
                let req = dev.subtree_req.as_mut().unwrap();
                if alloc_64bit {
                    req.prefetchable_window = Some((window_base, window_limit));
                } else {
                    req.memory_window = Some((window_base, window_limit));
                }

                // Recurse into children with this bridge's carved-out range.
                let req = dev
                    .subtree_req
                    .as_mut()
                    .expect("subtree_req must be populated by compute_subtree_state");
                let has_pins = if alloc_64bit {
                    req.constrained_base64.is_some()
                } else {
                    req.constrained_base32.is_some()
                };
                let (child_demands, child_gaps) = if alloc_64bit {
                    (&req.demands, &mut req.gaps64)
                } else {
                    (&req.demands, &mut req.gaps32)
                };
                if !has_pins {
                    child_gaps.push((window_base, window_limit + 1));
                }
                assign_subtree(&mut dev.children, child_demands, child_gaps, alloc_64bit);
            }
            Demand::SriovVfBars {
                dev_idx, bar_index, ..
            } => {
                // Record the assigned base address on the VF BAR.
                let sriov = devices[dev_idx]
                    .sriov
                    .as_mut()
                    .expect("SriovVfBars demand implies sriov is present");
                let vf_bar = sriov
                    .vf_bars
                    .iter_mut()
                    .find(|b| b.index == bar_index)
                    .expect("demand references a VF BAR that exists");
                vf_bar.address = Some(assign_addr);
            }
        }
    }
}

/// Program all assignments into config space.
///
/// Writes BAR addresses and bridge memory windows for every device in
/// the tree. This function assumes MMIO decode (MSE) has already been
/// cleared by the enumeration phase and does not modify the command
/// register.
pub async fn program_assignments(cfg: &mut impl PciConfigAccess, devices: &[DiscoveredDevice]) {
    for dev in devices {
        let devfn = crate::devfn(dev.device, dev.function);

        // Program BAR addresses.
        for bar in &dev.bars {
            let address = bar.address.unwrap();
            let offset = HeaderType00::BAR0.0 + (bar.index as u16) * 4;
            cfg.write_u32(dev.bus, devfn, offset, address as u32).await;

            if bar.is_64bit {
                let upper_offset = HeaderType00::BAR0.0 + ((bar.index + 1) as u16) * 4;
                cfg.write_u32(dev.bus, devfn, upper_offset, (address >> 32) as u32)
                    .await;
            }
        }

        // Program VF BAR addresses into the SR-IOV capability registers.
        if let Some(sriov) = &dev.sriov {
            for bar in &sriov.vf_bars {
                let address = bar.address.unwrap();
                let offset = sriov.cap_offset
                    + SriovExtendedCapabilityHeader::VF_BAR0.0
                    + (bar.index as u16) * 4;
                cfg.write_u32(dev.bus, devfn, offset, address as u32).await;

                if bar.is_64bit {
                    let upper_offset = offset + 4;
                    cfg.write_u32(dev.bus, devfn, upper_offset, (address >> 32) as u32)
                        .await;
                }
            }
        }

        // Program bridge windows. For bridges, explicitly disable any unused
        // window by writing base > limit so that the guest OS's probe
        // (write-readback) doesn't mistake zeroed registers for a valid window.
        if dev.is_bridge {
            let (memory_window, prefetchable_window) = dev
                .subtree_req
                .as_ref()
                .map(|r| (r.memory_window, r.prefetchable_window))
                .unwrap_or((None, None));

            // I/O window — we don't assign I/O BARs, so always disable.
            // Write zeros to the upper 16 bits (Secondary Status) since
            // those bits are W1C — writing back a read value would clear them.
            cfg.write_u32(
                dev.bus,
                devfn,
                HeaderType01::SEC_STATUS_IO_RANGE.0,
                0x0000_00F0,
            )
            .await;

            // Non-prefetchable memory window (32-bit only).
            let value = if let Some((base, limit)) = memory_window {
                let mem_base_reg = ((base >> 16) as u16 & MEMORY_BASE_LIMIT_ADDRESS_MASK) as u32;
                let mem_limit_reg = ((limit >> 16) as u16 & MEMORY_BASE_LIMIT_ADDRESS_MASK) as u32;
                mem_base_reg | (mem_limit_reg << 16)
            } else {
                0x0000_fff0
            };
            cfg.write_u32(dev.bus, devfn, HeaderType01::MEMORY_RANGE.0, value)
                .await;

            // Prefetchable memory window (64-bit capable).
            // Use base > limit to disable when no window is assigned.
            let (pf_range, pf_base_upper, pf_limit_upper) =
                if let Some((base, limit)) = prefetchable_window {
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
                dev.bus,
                devfn,
                HeaderType01::PREFETCH_LIMIT_UPPER.0,
                pf_limit_upper,
            )
            .await;
            cfg.write_u32(dev.bus, devfn, HeaderType01::PREFETCH_RANGE.0, pf_range)
                .await;
            cfg.write_u32(
                dev.bus,
                devfn,
                HeaderType01::PREFETCH_BASE_UPPER.0,
                pf_base_upper,
            )
            .await;
        }

        if dev.bars.is_empty() && dev.is_bridge {
            let memory_window = dev.subtree_req.as_ref().and_then(|r| r.memory_window);
            let prefetchable_window = dev.subtree_req.as_ref().and_then(|r| r.prefetchable_window);
            tracing::debug!(
                bus = dev.bus,
                device = dev.device,
                function = dev.function,
                ?dev.secondary_bus,
                ?dev.subordinate_bus,
                ?memory_window,
                ?prefetchable_window,
                "bridge programmed"
            );
        } else {
            for bar in &dev.bars {
                tracing::debug!(
                    bus = dev.bus,
                    device = dev.device,
                    function = dev.function,
                    bar_index = bar.index,
                    address = format_args!("{:#x}", bar.address.unwrap()),
                    size = format_args!("{:#x}", bar.size),
                    is_64bit = bar.is_64bit,
                    "BAR programmed"
                );
            }
        }

        if let Some(sriov) = &dev.sriov {
            for bar in &sriov.vf_bars {
                tracing::debug!(
                    bus = dev.bus,
                    device = dev.device,
                    function = dev.function,
                    vf_bar_index = bar.index,
                    address = format_args!("{:#x}", bar.address.unwrap()),
                    size = format_args!("{:#x}", bar.size),
                    total_vfs = sriov.total_vfs,
                    is_64bit = bar.is_64bit,
                    "VF BAR programmed"
                );
            }
        }

        // Recurse into children.
        Box::pin(program_assignments(cfg, &dev.children)).await;
    }
}

fn align_up(value: u64, alignment: u64) -> u64 {
    assert!(alignment.is_power_of_two());
    (value + alignment - 1) & !(alignment - 1)
}

fn align_down(value: u64, alignment: u64) -> u64 {
    assert!(alignment.is_power_of_two());
    value & !(alignment - 1)
}

/// Build a list of free gaps within [base, limit) given sorted,
/// non-overlapping fixed regions. Each gap is a (start, end) pair
/// where start is inclusive and end is exclusive.
fn build_gap_list(base: u64, limit: u64, fixed_regions: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut gaps = Vec::new();
    let mut cursor = base;
    for &(addr, size) in fixed_regions {
        if cursor < addr {
            gaps.push((cursor, addr));
        }
        cursor = cursor.max(addr + size);
    }
    if cursor < limit {
        gaps.push((cursor, limit));
    }
    gaps
}

/// Allocate `size` bytes with the given `alignment` from the first gap
/// that fits (first-fit). Returns the allocated address, or `None` if
/// no gap is large enough. Updates `gaps` in place.
fn allocate_from_gaps(gaps: &mut Vec<(u64, u64)>, size: u64, alignment: u64) -> Option<u64> {
    let gap_idx = gaps
        .iter()
        .position(|&(start, end)| align_up(start, alignment) + size <= end)?;

    let (gap_start, gap_end) = gaps[gap_idx];
    let addr = align_up(gap_start, alignment);
    let alloc_end = addr + size;

    gaps.remove(gap_idx);
    let mut insert_at = gap_idx;
    if gap_start < addr {
        gaps.insert(insert_at, (gap_start, addr));
        insert_at += 1;
    }
    if alloc_end < gap_end {
        gaps.insert(insert_at, (alloc_end, gap_end));
    }

    Some(addr)
}

/// Validate pinned BAR constraints: alignment, overlap, and aperture fit.
fn validate_pinned_bars(
    devices: &[DiscoveredDevice],
    params: &AssignmentParams,
) -> Result<(), AssignmentError> {
    let mut all_pinned: Vec<(u64, u64, u8, u8, u8, u8, bool)> = Vec::new();
    collect_pinned_bars(devices, &mut all_pinned);

    // Check natural alignment.
    for &(addr, size, bus, device, function, bar_index, _) in &all_pinned {
        if addr % size != 0 {
            return Err(AssignmentError::PinnedBarMisaligned {
                bus,
                device,
                function,
                bar_index,
                address: addr,
                required_alignment: size,
            });
        }
    }

    // Check for overlap within each pool.
    for is_mem64 in [false, true] {
        let mut pool: Vec<_> = all_pinned
            .iter()
            .filter(|p| p.6 == is_mem64)
            .copied()
            .collect();
        pool.sort_by_key(|p| p.0);
        for w in pool.windows(2) {
            if w[0].0 + w[0].1 > w[1].0 {
                return Err(AssignmentError::PinnedBarOverlap {
                    first_address: w[0].0,
                    first_end: w[0].0 + w[0].1,
                    second_address: w[1].0,
                    second_end: w[1].0 + w[1].1,
                });
            }
        }
    }

    // Check aperture containment.
    for &(addr, size, bus, device, function, bar_index, is_mem64) in &all_pinned {
        let aperture = if is_mem64 {
            params.high_mmio.or(params.low_mmio)
        } else {
            params.low_mmio
        };
        let fits = aperture.is_some_and(|a| addr >= a.base && addr + size <= a.base + a.len);
        if !fits {
            return Err(AssignmentError::PinnedBarOutOfAperture {
                bus,
                device,
                function,
                bar_index,
                address: addr,
                size,
                aperture: if is_mem64 { "high_mmio" } else { "low_mmio" },
            });
        }
    }

    Ok(())
}

/// Recursively collect all pinned BARs from the device tree.
/// Each entry: (address, size, bus, device, function, bar_index, is_mem64).
fn collect_pinned_bars(
    devices: &[DiscoveredDevice],
    out: &mut Vec<(u64, u64, u8, u8, u8, u8, bool)>,
) {
    for dev in devices {
        for bar in &dev.bars {
            if let Some(addr) = bar.pinned_address {
                // Pinned BARs below 4 GB are treated as mem32.
                let is_mem64 = addr >= 0x1_0000_0000 && is_mem64_bar(bar);
                out.push((
                    addr,
                    bar.size,
                    dev.bus,
                    dev.device,
                    dev.function,
                    bar.index,
                    is_mem64,
                ));
            }
        }
        collect_pinned_bars(&dev.children, out);
    }
}
