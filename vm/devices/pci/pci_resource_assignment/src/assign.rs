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

/// Sizing requirement for a subtree (bridge or root), computed during
/// the bottom-up pass.
#[derive(Debug, Clone)]
struct SubtreeLayout {
    /// Total aligned size needed in the mem32 (non-prefetchable) pool.
    mem32: u64,
    /// Total aligned size needed in the mem64 (prefetchable) pool.
    mem64: u64,
    /// Required alignment for the mem32 pool (max of bridge granularity
    /// and the largest BAR in the subtree).
    align32: u64,
    /// Required alignment for the mem64 pool.
    align64: u64,
    /// Sorted demands for this level's devices.
    demands: Vec<Demand>,
    /// Offset within the pool for each demand (parallel to `demands`).
    /// Always relative to the pool base (0 for unconstrained pools,
    /// `constrained_base` for constrained pools). The final address
    /// is `pool_base + offset`.
    offsets: Vec<u64>,
    /// If pinned demands exist in the mem32 pool, the required base address
    /// for this subtree's window (align_down of the lowest pinned address).
    constrained_base32: Option<u64>,
    /// If pinned demands exist in the mem64 pool, the required base address
    /// for this subtree's window.
    constrained_base64: Option<u64>,
}

/// All bridge-specific state populated by the assignment phase.
///
/// Groups the subtree sizing (computed bottom-up) and the assigned
/// bridge windows (set top-down) so that [`DiscoveredDevice`] only
/// needs a single `Option` field for assignment state.
#[derive(Debug, Clone)]
pub(crate) struct BridgeAssignment {
    /// Subtree sizing computed during the bottom-up pass.
    layout: SubtreeLayout,
    /// Assigned non-prefetchable bridge window (base, limit).
    memory_window: Option<(u64, u64)>,
    /// Assigned prefetchable bridge window (base, limit).
    prefetchable_window: Option<(u64, u64)>,
}

/// A single resource demand at one level of the PCI tree.
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
/// 1. **Bottom-up layout**: Each bridge computes the total aligned
///    resource requirement of its subtree and allocates offsets for
///    each demand within the subtree window.
/// 2. **Top-down fixup**: Starting from the aperture base, each
///    bridge's window base is resolved and added to the pre-computed
///    offsets to produce final addresses.
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
    // Validate pinned BAR constraints before layout, since the layout
    // pass builds gap lists from pinned positions and assumes they are
    // valid (naturally aligned, non-overlapping, within apertures).
    validate_pinned_bars(devices, params)?;

    // Step 1: Bottom-up — compute total resource requirements and
    // allocate pool-local positions for each demand.
    let root_sizing = compute_subtree_layout(devices);

    // Step 2: Top-down — determine aperture base addresses and apply
    // the pre-computed placements to devices.
    let mut mem32_end: Option<u64> = None;

    if root_sizing.mem32 > 0 {
        // 32-bit BARs and non-prefetchable bridge windows are inherently
        // 32-bit, so the aperture must be below 4 GB. Do not fall back
        // to high_mmio — placing 32-bit BARs above 4 GB would silently
        // truncate addresses.
        let base = resolve_pool_base(
            root_sizing.constrained_base32,
            root_sizing.align32,
            root_sizing.mem32,
            params.low_mmio,
            false,
            None,
        )?;
        apply_offsets(
            devices,
            &root_sizing.demands,
            &root_sizing.offsets,
            base,
            false,
        );

        mem32_end = Some(base + root_sizing.mem32);
    }

    if root_sizing.mem64 > 0 {
        // If sharing the same aperture as mem32, start after mem32.
        let after_mem32 = mem32_end.filter(|_| params.high_mmio.is_empty());

        let aperture = if params.high_mmio.is_empty() {
            params.low_mmio
        } else {
            params.high_mmio
        };
        let base = resolve_pool_base(
            root_sizing.constrained_base64,
            root_sizing.align64,
            root_sizing.mem64,
            aperture,
            true,
            after_mem32,
        )?;
        apply_offsets(
            devices,
            &root_sizing.demands,
            &root_sizing.offsets,
            base,
            true,
        );
    }

    validate_assignments(devices, params);
    Ok(())
}

/// Verify that all assigned BAR addresses fall within the provided apertures.
fn validate_assignments(devices: &[DiscoveredDevice], params: &AssignmentParams) {
    for dev in devices {
        for bar in &dev.bars {
            let address = bar.address.unwrap();
            if let Some(pinned) = bar.pinned_address {
                assert_eq!(
                    address,
                    pinned,
                    "BAR {bus:02x}:{device:02x}.{func} index {idx} assigned {addr:#x} \
                     but pinned to {pinned:#x}",
                    bus = dev.bus,
                    device = dev.device,
                    func = dev.function,
                    idx = bar.index,
                    addr = address,
                );
            }
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
        if let Some(ba) = &dev.bridge_assignment {
            if let Some((base, limit)) = ba.memory_window {
                let size = limit - base + 1;
                assert!(
                    params.low_mmio.contains_addr(base)
                        && params.low_mmio.contains_addr(base + size - 1),
                    "bridge {bus:02x}:{device:02x}.{func} memory window \
                     {base:#x}..={limit:#x} exceeds low_mmio aperture",
                    bus = dev.bus,
                    device = dev.device,
                    func = dev.function,
                );
            }
            if let Some((base, limit)) = ba.prefetchable_window {
                let size = limit - base + 1;
                let in_low = params.low_mmio.contains_addr(base)
                    && params.low_mmio.contains_addr(base + size - 1);
                let in_high = params.high_mmio.contains_addr(base)
                    && params.high_mmio.contains_addr(base + size - 1);
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
                let child_ba = child.bridge_assignment.as_ref();
                if let (Some((cb, cl)), Some((pb, pl))) =
                    (child_ba.and_then(|b| b.memory_window), ba.memory_window)
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
                if let (Some((cb, cl)), Some((pb, pl))) = (
                    child_ba.and_then(|b| b.prefetchable_window),
                    ba.prefetchable_window,
                ) {
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
    let in_low = address >= params.low_mmio.start() && bar_end <= params.low_mmio.end();
    let in_high = address >= params.high_mmio.start() && bar_end <= params.high_mmio.end();
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
fn bar_is_mem64(bar: &crate::enumerate::DiscoveredBar) -> bool {
    if let Some(addr) = bar.pinned_address {
        addr >= 0x1_0000_0000 && bar.is_64bit && bar.is_prefetchable
    } else {
        bar.is_64bit && bar.is_prefetchable
    }
}

/// Bottom-up: compute the layout for a list of devices (which may be
/// the root level or children behind a bridge).
///
/// Builds the sorted demand list, allocates pool-local offsets for each
/// demand, and computes the total window size for each pool.
fn compute_subtree_layout(devices: &mut [DiscoveredDevice]) -> SubtreeLayout {
    let mut demands: Vec<Demand> = Vec::new();

    for (i, dev) in devices.iter_mut().enumerate() {
        for bar in &dev.bars {
            let is_mem64 = bar_is_mem64(bar);
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
                    is_mem64: bar.is_64bit && bar.is_prefetchable,
                });
            }
        }

        if dev.is_bridge {
            let child_req = compute_subtree_layout(&mut dev.children);
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
            dev.bridge_assignment = Some(BridgeAssignment {
                layout: child_req,
                memory_window: None,
                prefetchable_window: None,
            });
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

    // Build gap lists for each pool and allocate all demands in a
    // single pass. All placements are stored as offsets from the pool
    // base (0 for unconstrained, constrained_base for constrained).
    let mut pool32 = PoolState::new(&mut pin32);
    let mut pool64 = PoolState::new(&mut pin64);

    let mut offsets = vec![0u64; demands.len()];
    for (i, d) in demands.iter().enumerate() {
        let pool = if d.is_mem64() {
            &mut pool64
        } else {
            &mut pool32
        };
        pool.align = pool.align.max(d.alignment());

        let addr = if let Some((addr, _)) = d.fixed_position() {
            addr
        } else {
            allocate_from_gaps(&mut pool.gaps, d.size(), d.alignment())
                .expect("gap list has open-ended tail — allocation cannot fail")
        };
        offsets[i] = addr - pool.constrained_base.unwrap_or(0);
    }

    // Derive pool sizes from the furthest allocation endpoint.
    let mut mem32 = 0u64;
    let mut mem64 = 0u64;
    for (i, d) in demands.iter().enumerate() {
        let endpoint = offsets[i] + d.size();
        if d.is_mem64() {
            mem64 = mem64.max(endpoint);
        } else {
            mem32 = mem32.max(endpoint);
        }
    }
    if mem32 > 0 {
        mem32 = align_up(mem32, BRIDGE_WINDOW_ALIGN);
    }
    if mem64 > 0 {
        mem64 = align_up(mem64, BRIDGE_WINDOW_ALIGN);
    }

    SubtreeLayout {
        mem32,
        mem64,
        align32: pool32.align,
        align64: pool64.align,
        demands,
        offsets,
        constrained_base32: pool32.constrained_base,
        constrained_base64: pool64.constrained_base,
    }
}

/// Per-pool state: gap list and alignment, built from pinned demands.
struct PoolState {
    /// Required alignment (max of bridge granularity and largest demand).
    align: u64,
    /// If pinned demands exist, the required window base address.
    constrained_base: Option<u64>,
    /// Free gap list for allocation.
    gaps: Vec<(u64, u64)>,
}

impl PoolState {
    /// Build the gap list for one pool. For constrained pools (pins
    /// present), gaps cover spaces between pins plus an open-ended tail.
    /// For unconstrained pools, a single `[0, u64::MAX)` gap.
    fn new(pins: &mut [(u64, u64)]) -> Self {
        pins.sort_by_key(|&(a, _)| a);

        if pins.is_empty() {
            return Self {
                align: BRIDGE_WINDOW_ALIGN,
                constrained_base: None,
                gaps: vec![(0, u64::MAX)],
            };
        }

        let min_addr = pins.iter().map(|(a, _)| *a).min().unwrap();
        let max_end = pins.iter().map(|(a, s)| a + s).max().unwrap();
        let base = align_down(min_addr, BRIDGE_WINDOW_ALIGN);
        let mut gaps = build_gap_list(base, max_end, pins);
        // Tail gap for dynamic demands that don't fit between pins.
        gaps.push((max_end, u64::MAX));

        Self {
            align: BRIDGE_WINDOW_ALIGN,
            constrained_base: Some(base),
            gaps,
        }
    }
}

/// Determine the base address for a pool within an aperture, validating
/// that the required size fits.
///
/// Returns the effective base address on success.
fn resolve_pool_base(
    constrained_base: Option<u64>,
    alignment: u64,
    required: u64,
    aperture: memory_range::MemoryRange,
    is_mem64: bool,
    after: Option<u64>,
) -> Result<u64, AssignmentError> {
    let aperture_name = if is_mem64 { "high_mmio" } else { "low_mmio" };
    let base = if let Some(cbase) = constrained_base {
        cbase
    } else if let Some(end) = after {
        align_up(end, alignment)
    } else {
        align_up(aperture.start(), alignment)
    };

    // Bridge windows are 1 MB granular, so the constrained base
    // (align_down of the lowest pinned address) can precede the
    // aperture. Reject this rather than placing BARs outside it.
    if base < aperture.start() {
        return Err(AssignmentError::MmioExhaustion {
            required,
            available: aperture.len(),
            aperture: aperture_name,
        });
    }
    let available = aperture.end().saturating_sub(base);
    if required > available {
        return Err(AssignmentError::MmioExhaustion {
            required,
            available,
            aperture: aperture_name,
        });
    }

    Ok(base)
}

/// Top-down: apply pre-computed offsets to devices.
///
/// `base` is the pool's starting address (aperture-derived for the root,
/// or the bridge window base for children). Each demand's final address
/// is `base + offset`.
fn apply_offsets(
    devices: &mut [DiscoveredDevice],
    demands: &[Demand],
    offsets: &[u64],
    base: u64,
    is_mem64: bool,
) {
    for (demand, &offset) in demands.iter().zip(offsets) {
        if demand.is_mem64() != is_mem64 {
            continue;
        }

        let final_addr = base + offset;

        match *demand {
            Demand::Bar {
                dev_idx, bar_index, ..
            } => {
                let bar = devices[dev_idx]
                    .bars
                    .iter_mut()
                    .find(|b| b.index == bar_index)
                    .expect("demand references a BAR that exists");
                bar.address = Some(final_addr);
            }
            Demand::BridgeSubtree { dev_idx, size, .. } => {
                let dev = &mut devices[dev_idx];
                let window_base = final_addr;
                let window_limit = final_addr + size - 1;
                let ba = dev
                    .bridge_assignment
                    .as_mut()
                    .expect("bridge_assignment must be populated by compute_subtree_layout");
                if is_mem64 {
                    ba.prefetchable_window = Some((window_base, window_limit));
                } else {
                    ba.memory_window = Some((window_base, window_limit));
                }

                apply_offsets(
                    &mut dev.children,
                    &ba.layout.demands,
                    &ba.layout.offsets,
                    window_base,
                    is_mem64,
                );
            }
            Demand::SriovVfBars {
                dev_idx, bar_index, ..
            } => {
                let sriov = devices[dev_idx]
                    .sriov
                    .as_mut()
                    .expect("SriovVfBars demand implies sriov is present");
                let vf_bar = sriov
                    .vf_bars
                    .iter_mut()
                    .find(|b| b.index == bar_index)
                    .expect("demand references a VF BAR that exists");
                vf_bar.address = Some(final_addr);
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
            let ba = dev.bridge_assignment.as_ref();
            let (memory_window, prefetchable_window) = ba
                .map(|b| (b.memory_window, b.prefetchable_window))
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
            let ba = dev.bridge_assignment.as_ref();
            let memory_window = ba.and_then(|b| b.memory_window);
            let prefetchable_window = ba.and_then(|b| b.prefetchable_window);
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
    let gap_idx = gaps.iter().position(|&(start, end)| {
        let aligned = align_up(start, alignment);
        aligned <= end && end - aligned >= size
    })?;

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

#[derive(Clone, Copy)]
struct PinnedBar {
    addr: u64,
    size: u64,
    bus: u8,
    device: u8,
    function: u8,
    bar_index: u8,
    is_mem64: bool,
}

/// Validate pinned BAR constraints: alignment, overlap, and aperture fit.
fn validate_pinned_bars(
    devices: &[DiscoveredDevice],
    params: &AssignmentParams,
) -> Result<(), AssignmentError> {
    let mut all_pinned = Vec::new();
    collect_pinned_bars(devices, &mut all_pinned);

    // Check natural alignment.
    for p in &all_pinned {
        if p.addr % p.size != 0 {
            return Err(AssignmentError::PinnedBarMisaligned {
                bus: p.bus,
                device: p.device,
                function: p.function,
                bar_index: p.bar_index,
                address: p.addr,
                required_alignment: p.size,
            });
        }
    }

    // Check for overlap within each pool.
    for is_mem64 in [false, true] {
        let mut pool: Vec<_> = all_pinned
            .iter()
            .filter(|p| p.is_mem64 == is_mem64)
            .copied()
            .collect();
        pool.sort_by_key(|p| p.addr);
        for [a, b] in pool.array_windows::<2>() {
            let first_end = a.addr.saturating_add(a.size);
            if first_end > b.addr {
                return Err(AssignmentError::PinnedBarOverlap {
                    first_address: a.addr,
                    first_end,
                    second_address: b.addr,
                    second_end: b.addr.saturating_add(b.size),
                });
            }
        }
    }

    // Check aperture containment.
    for p in &all_pinned {
        let (aperture, aperture_name) = if p.is_mem64 && !params.high_mmio.is_empty() {
            (params.high_mmio, "high_mmio")
        } else {
            (params.low_mmio, "low_mmio")
        };
        let bar_end = p.addr.saturating_add(p.size);
        let fits = p.addr >= aperture.start() && bar_end <= aperture.end();
        if !fits {
            return Err(AssignmentError::PinnedBarOutOfAperture {
                bus: p.bus,
                device: p.device,
                function: p.function,
                bar_index: p.bar_index,
                address: p.addr,
                size: p.size,
                aperture: aperture_name,
            });
        }
    }

    Ok(())
}

/// Recursively collect all pinned BARs from the device tree.
fn collect_pinned_bars(devices: &[DiscoveredDevice], out: &mut Vec<PinnedBar>) {
    for dev in devices {
        for bar in &dev.bars {
            if let Some(addr) = bar.pinned_address {
                let is_mem64 = bar_is_mem64(bar);
                out.push(PinnedBar {
                    addr,
                    size: bar.size,
                    bus: dev.bus,
                    device: dev.device,
                    function: dev.function,
                    bar_index: bar.index,
                    is_mem64,
                });
            }
        }
        collect_pinned_bars(&dev.children, out);
    }
}
