// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Phase 1: PCI bus enumeration and BAR size probing.

use crate::AssignmentError;
use crate::AssignmentParams;
use crate::PciConfigAccess;
use pci_core::spec::caps::EXT_CAP_START;
use pci_core::spec::caps::ExtendedCapabilityId;
use pci_core::spec::caps::sriov::SriovExtendedCapabilityHeader;
use pci_core::spec::cfg_space::BarEncodingBits;
use pci_core::spec::cfg_space::BistHeader;
use pci_core::spec::cfg_space::Command;
use pci_core::spec::cfg_space::CommonHeader;
use pci_core::spec::cfg_space::HeaderType00;
use pci_core::spec::cfg_space::HeaderType01;

/// A discovered PCI device or bridge with probed BAR sizes.
#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub is_bridge: bool,
    pub bars: Vec<DiscoveredBar>,
    /// For bridges: children behind this bridge.
    pub children: Vec<DiscoveredDevice>,
    /// For bridges: the secondary bus number assigned during enumeration.
    pub secondary_bus: Option<u8>,
    /// For bridges: the subordinate bus number assigned during enumeration.
    pub subordinate_bus: Option<u8>,
    /// For SR-IOV PFs: total VFs and per-VF BAR sizes.
    pub(crate) sriov: Option<DiscoveredSriov>,
    /// Bridge assignment state (sizing + windows), populated by the
    /// assignment pass. `None` for endpoints and before assignment runs.
    pub(crate) bridge_assignment: Option<crate::assign::BridgeAssignment>,
}

/// A discovered BAR with its size.
#[derive(Debug, Clone)]
pub struct DiscoveredBar {
    pub index: u8,
    pub size: u64,
    pub is_64bit: bool,
    pub is_prefetchable: bool,
    /// Assigned base address (populated by the assignment pass).
    pub(crate) address: Option<u64>,
    /// Pre-programmed BAR address to preserve (set when `preserve_bars` is
    /// enabled and the BAR contained a non-zero address before probing).
    pub pinned_address: Option<u64>,
}

/// SR-IOV information for a PF.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredSriov {
    /// Config space offset of the SR-IOV extended capability.
    pub cap_offset: u16,
    /// Total number of VFs.
    pub total_vfs: u16,
    /// Per-VF BAR sizes.
    pub vf_bars: Vec<DiscoveredBar>,
}

/// Enumerate all devices starting from the host bridge's start bus,
/// assigning bus numbers to bridges and probing BAR sizes.
///
/// As a side effect, MMIO decode (MSE) is cleared in each device's
/// command register. This is necessary to safely probe BAR sizes, and
/// the bit is intentionally left cleared so that devices do not decode
/// stale addresses. The caller must use [`crate::assign::program_assignments`]
/// to write valid BAR addresses before re-enabling MMIO decode.
pub async fn enumerate_and_probe(
    cfg: &mut impl PciConfigAccess,
    params: &AssignmentParams,
) -> Result<Vec<DiscoveredDevice>, AssignmentError> {
    let mut next_bus = params.start_bus as u16 + 1;
    scan_bus(
        cfg,
        params.start_bus,
        params.end_bus,
        &mut next_bus,
        params.preserve_bars,
    )
    .await
}

/// Scan a single bus (non-recursive helper that does DFS via inner calls).
/// This is called for secondary buses behind bridges. It's not async-recursive
/// itself because we use the pattern of scanning children inline.
async fn scan_bus(
    cfg: &mut impl PciConfigAccess,
    bus: u8,
    end_bus: u8,
    next_bus: &mut u16,
    preserve_bars: bool,
) -> Result<Vec<DiscoveredDevice>, AssignmentError> {
    let mut devices = Vec::new();

    for device_num in 0..32u8 {
        let vendor = cfg
            .read_u32(
                bus,
                crate::devfn(device_num, 0),
                CommonHeader::DEVICE_VENDOR.0,
            )
            .await;
        if vendor == !0u32 {
            continue;
        }

        let bist_header_raw = cfg
            .read_u32(
                bus,
                crate::devfn(device_num, 0),
                HeaderType00::BIST_HEADER.0,
            )
            .await;
        let bist = BistHeader::from(bist_header_raw);
        let header_type = bist.header_type();
        let multi_function = bist.multi_function();

        let max_func = if multi_function { 8 } else { 1 };

        for function in 0..max_func {
            let devfn = crate::devfn(device_num, function);

            if function > 0 {
                let vendor = cfg
                    .read_u32(bus, devfn, CommonHeader::DEVICE_VENDOR.0)
                    .await;
                if vendor == !0u32 {
                    continue;
                }
            }

            let func_header = if function > 0 {
                let bh = cfg.read_u32(bus, devfn, HeaderType00::BIST_HEADER.0).await;
                BistHeader::from(bh).header_type()
            } else {
                header_type
            };

            let is_bridge = func_header == 1;
            let bars = probe_bars(cfg, bus, devfn, is_bridge, preserve_bars).await;

            let mut dev = DiscoveredDevice {
                bus,
                device: device_num,
                function,
                is_bridge,
                bars,
                children: Vec::new(),
                secondary_bus: None,
                subordinate_bus: None,
                sriov: None,
                bridge_assignment: None,
            };

            if is_bridge {
                if *next_bus > end_bus as u16 {
                    return Err(AssignmentError::BusExhaustion {
                        bus,
                        device: device_num,
                        function,
                    });
                }
                let secondary = *next_bus as u8;
                *next_bus += 1;

                let bus_reg = (bus as u32) | ((secondary as u32) << 8) | ((end_bus as u32) << 16);
                cfg.write_u32(bus, devfn, HeaderType01::LATENCY_BUS_NUMBERS.0, bus_reg)
                    .await;

                // NOTE: This is still logically recursive (scan_bus calls
                // itself indirectly via this path), but the Rust compiler
                // handles it because each call is a separate monomorphized
                // async block that goes through the same function, and we
                // box it to avoid infinite-size futures.
                let children =
                    Box::pin(scan_bus(cfg, secondary, end_bus, next_bus, preserve_bars)).await?;

                let subordinate = (*next_bus - 1).max(secondary as u16) as u8;
                let bus_reg =
                    (bus as u32) | ((secondary as u32) << 8) | ((subordinate as u32) << 16);
                cfg.write_u32(bus, devfn, HeaderType01::LATENCY_BUS_NUMBERS.0, bus_reg)
                    .await;

                dev.secondary_bus = Some(secondary);
                dev.subordinate_bus = Some(subordinate);
                dev.children = children;

                tracing::debug!(
                    bus,
                    device = device_num,
                    function,
                    secondary,
                    subordinate,
                    "bridge enumerated"
                );
            } else {
                // Probe SR-IOV capability for bus reservation and VF BAR sizes.
                if let Some(sriov_result) = probe_sriov(cfg, bus, devfn, preserve_bars).await {
                    let max_vf_bus = sriov_result.max_vf_bus;
                    if max_vf_bus > end_bus as u16 {
                        return Err(AssignmentError::BusExhaustion {
                            bus,
                            device: device_num,
                            function,
                        });
                    }
                    // VF bus numbers are fixed by the device's VF Offset
                    // and VF Stride. VFs that stay on the PF's own bus
                    // don't need any bus reservation. VFs that extend to
                    // other buses must not collide with buses already
                    // assigned to sibling bridges.
                    if max_vf_bus > bus as u16 {
                        if max_vf_bus < *next_bus {
                            return Err(AssignmentError::SriovBusConflict {
                                bus,
                                device: device_num,
                                function,
                                max_vf_bus,
                                next_bus: *next_bus,
                            });
                        }
                        *next_bus = max_vf_bus + 1;
                    }

                    dev.sriov = Some(DiscoveredSriov {
                        cap_offset: sriov_result.cap_offset,
                        total_vfs: sriov_result.total_vfs,
                        vf_bars: sriov_result.vf_bars,
                    });
                }

                tracing::debug!(
                    bus,
                    device = device_num,
                    function,
                    bar_count = dev.bars.len(),
                    "endpoint enumerated"
                );
            }

            devices.push(dev);
        }
    }

    Ok(devices)
}

/// Probe BAR sizes for a device by writing all-ones and reading back.
///
/// Disables MMIO decode (MSE) in the device's command register before
/// probing and does not restore it. BAR registers are also left in an
/// undefined state. The caller is responsible for programming valid BAR
/// addresses and re-enabling MMIO decode afterward.
async fn probe_bars(
    cfg: &mut impl PciConfigAccess,
    bus: u8,
    devfn: u8,
    is_bridge: bool,
    preserve_bars: bool,
) -> Vec<DiscoveredBar> {
    let max_bars: u8 = if is_bridge { 2 } else { 6 };

    // Disable MMIO decode so that writing all-ones to BARs during
    // probing does not cause the device to decode a bogus address range.
    // The command register is left with MMIO disabled; program_assignments
    // will enable it once valid addresses have been programmed.
    let cmd = cfg
        .read_u32(bus, devfn, CommonHeader::STATUS_COMMAND.0)
        .await;
    let command = Command::from(cmd as u16);
    if command.mmio_enabled() {
        // Status bits are W1C, so avoid writing them.
        cfg.write_u32(
            bus,
            devfn,
            CommonHeader::STATUS_COMMAND.0,
            command.with_mmio_enabled(false).into_bits().into(),
        )
        .await;
    }

    probe_bar_range(
        cfg,
        bus,
        devfn,
        HeaderType00::BAR0.0,
        max_bars,
        preserve_bars,
    )
    .await
}

/// Probe VF BAR sizes from the SR-IOV capability's VF BAR registers.
///
/// VF BARs are at offsets 0x24–0x38 within the SR-IOV capability, and
/// use the same write-all-ones/readback protocol as regular BARs.
async fn probe_vf_bars(
    cfg: &mut impl PciConfigAccess,
    bus: u8,
    devfn: u8,
    sriov_offset: u16,
    preserve_bars: bool,
) -> Vec<DiscoveredBar> {
    probe_bar_range(
        cfg,
        bus,
        devfn,
        sriov_offset + SriovExtendedCapabilityHeader::VF_BAR0.0,
        6,
        preserve_bars,
    )
    .await
}

/// Result of probing an SR-IOV capability.
pub(crate) struct SriovProbeResult {
    /// Config space offset of the SR-IOV extended capability.
    pub cap_offset: u16,
    /// Highest bus number a VF could land on.
    pub max_vf_bus: u16,
    /// Total number of VFs.
    pub total_vfs: u16,
    /// VF BAR sizes discovered by probing (same format as device BARs).
    pub vf_bars: Vec<DiscoveredBar>,
}

/// Probe SR-IOV capability to determine bus requirements and VF BAR sizes.
/// Returns `None` if the device has no SR-IOV capability or has no VFs.
async fn probe_sriov(
    cfg: &mut impl PciConfigAccess,
    bus: u8,
    devfn: u8,
    preserve_bars: bool,
) -> Option<SriovProbeResult> {
    // Walk extended capabilities starting at 0x100.
    let mut offset = EXT_CAP_START;
    loop {
        if offset < EXT_CAP_START || offset & 0x3 != 0 {
            break;
        }
        let header = cfg.read_u32(bus, devfn, offset).await;
        if header == 0 || header == !0u32 {
            break;
        }
        let cap_id = (header & 0xFFFF) as u16;
        let next = ((header >> 20) & 0xFFC) as u16;

        if cap_id == ExtendedCapabilityId::SRIOV.0 {
            // Read TotalVFs.
            let vfs_dword = cfg
                .read_u32(
                    bus,
                    devfn,
                    offset + SriovExtendedCapabilityHeader::INITIAL_TOTAL_VFS.0,
                )
                .await;
            let total_vfs = (vfs_dword >> 16) as u16;
            if total_vfs == 0 {
                return None;
            }

            // Read VF Offset and VF Stride.
            let offset_stride = cfg
                .read_u32(
                    bus,
                    devfn,
                    offset + SriovExtendedCapabilityHeader::VF_OFFSET_STRIDE.0,
                )
                .await;
            let vf_offset = offset_stride as u16;
            let vf_stride = (offset_stride >> 16) as u16;

            if vf_stride == 0 {
                return None;
            }

            // Compute the BDF of the last VF. Use checked arithmetic
            // since these values come from hardware and could overflow.
            // A routing ID is 16 bits (bus:8 | devfn:8).
            let pf_rid = (bus as u16) << 8 | devfn as u16;
            let last_vf_rid = (total_vfs - 1)
                .checked_mul(vf_stride)?
                .checked_add(vf_offset)?
                .checked_add(pf_rid)?;
            let max_vf_bus = (last_vf_rid >> 8) as u16;

            // Probe VF BAR sizes (same write-all-ones/readback technique).
            let vf_bars = probe_vf_bars(cfg, bus, devfn, offset, preserve_bars).await;

            return Some(SriovProbeResult {
                cap_offset: offset,
                max_vf_bus,
                total_vfs,
                vf_bars,
            });
        }

        if next == 0 || next <= offset {
            break;
        }
        offset = next;
    }
    None
}

/// Probe BAR sizes for a range of BAR registers starting at `base_offset`.
///
/// Writes all-ones to each BAR and reads back to determine size. BAR
/// registers are left in an undefined state after probing.
///
/// When `preserve_bars` is true, reads each BAR's current value before
/// probing. If non-zero, records it as `pinned_address` on the
/// resulting [`DiscoveredBar`].
async fn probe_bar_range(
    cfg: &mut impl PciConfigAccess,
    bus: u8,
    devfn: u8,
    base_offset: u16,
    max_bars: u8,
    preserve_bars: bool,
) -> Vec<DiscoveredBar> {
    let mut bars = Vec::new();

    let mut i = 0u8;
    while i < max_bars {
        let offset = base_offset + (i as u16) * 4;

        // Read the current BAR value before probing (for preserve_bars).
        let original_lower = if preserve_bars {
            cfg.read_u32(bus, devfn, offset).await
        } else {
            0
        };

        // Write all-ones to probe size.
        cfg.write_u32(bus, devfn, offset, !0u32).await;
        let readback = cfg.read_u32(bus, devfn, offset).await;

        if readback == 0 {
            // BAR not implemented.
            i += 1;
            continue;
        }

        let is_io = BarEncodingBits::from(readback).use_pio();
        if is_io {
            // Skip I/O BARs.
            i += 1;
            continue;
        }

        let encoding = BarEncodingBits::from(readback);
        let is_64bit = encoding.type_64_bit();
        let is_prefetchable = encoding.prefetchable();

        let (size, pinned_address) = if is_64bit && (i + 1) < max_bars {
            // Probe upper 32 bits.
            let upper_offset = base_offset + ((i + 1) as u16) * 4;

            let original_upper = if preserve_bars {
                cfg.read_u32(bus, devfn, upper_offset).await
            } else {
                0
            };

            cfg.write_u32(bus, devfn, upper_offset, !0u32).await;
            let upper_readback = cfg.read_u32(bus, devfn, upper_offset).await;

            let mask = ((upper_readback as u64) << 32) | (readback as u64 & !0xF);
            if mask == 0 {
                i += 2;
                continue;
            }
            let size = (!mask).wrapping_add(1);

            let pinned = if preserve_bars {
                let addr = ((original_upper as u64) << 32) | ((original_lower & !0xF) as u64);
                (addr != 0).then_some(addr)
            } else {
                None
            };

            (size, pinned)
        } else {
            let mask = readback & !0xF;
            if mask == 0 {
                i += 1;
                continue;
            }
            let size = (!(mask as u64 | (!0u64 << 32))).wrapping_add(1);

            let pinned = if preserve_bars {
                let addr = (original_lower & !0xF) as u64;
                (addr != 0).then_some(addr)
            } else {
                None
            };

            (size, pinned)
        };

        if size > 0 {
            bars.push(DiscoveredBar {
                index: i,
                size,
                is_64bit,
                is_prefetchable,
                address: None,
                pinned_address,
            });
        }

        if is_64bit {
            i += 2;
        } else {
            i += 1;
        }
    }

    bars
}
