// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Phase 1: PCI bus enumeration and BAR size probing.

use crate::AssignmentError;
use crate::AssignmentParams;
use crate::PciConfigAccess;
use pci_core::spec::caps::EXT_CAP_START;
use pci_core::spec::caps::ExtendedCapabilityId;
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
    #[expect(dead_code)] // stored for future use in Phase 2+ diagnostics
    pub(crate) header_type: u8,
    pub is_bridge: bool,
    #[expect(dead_code)] // stored for future use in Phase 2+ diagnostics
    pub(crate) is_multi_function: bool,
    pub bars: Vec<DiscoveredBar>,
    /// For bridges: children behind this bridge.
    pub children: Vec<DiscoveredDevice>,
    /// For bridges: the secondary bus number assigned during enumeration.
    pub secondary_bus: Option<u8>,
    /// For bridges: the subordinate bus number assigned during enumeration.
    pub subordinate_bus: Option<u8>,
}

/// A discovered BAR with its size.
#[derive(Debug, Clone)]
pub struct DiscoveredBar {
    pub index: u8,
    pub size: u64,
    pub is_64bit: bool,
    pub is_prefetchable: bool,
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
    let mut next_bus = params.start_bus.wrapping_add(1);
    scan_bus(cfg, params.start_bus, params.end_bus, &mut next_bus).await
}

/// Scan a single bus (non-recursive helper that does DFS via inner calls).
/// This is called for secondary buses behind bridges. It's not async-recursive
/// itself because we use the pattern of scanning children inline.
async fn scan_bus(
    cfg: &mut impl PciConfigAccess,
    bus: u8,
    end_bus: u8,
    next_bus: &mut u8,
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
            let bars = probe_bars(cfg, bus, devfn, is_bridge).await;

            let mut dev = DiscoveredDevice {
                bus,
                device: device_num,
                function,
                header_type: func_header,
                is_bridge,
                is_multi_function: multi_function,
                bars,
                children: Vec::new(),
                secondary_bus: None,
                subordinate_bus: None,
            };

            if is_bridge {
                if *next_bus > end_bus {
                    return Err(AssignmentError::BusExhaustion {
                        bus,
                        device: device_num,
                        function,
                    });
                }
                let secondary = *next_bus;
                *next_bus = next_bus.wrapping_add(1);

                let bus_reg = (bus as u32) | ((secondary as u32) << 8) | ((end_bus as u32) << 16);
                cfg.write_u32(bus, devfn, HeaderType01::LATENCY_BUS_NUMBERS.0, bus_reg)
                    .await;

                // NOTE: This is still logically recursive (scan_bus calls
                // itself indirectly via this path), but the Rust compiler
                // handles it because each call is a separate monomorphized
                // async block that goes through the same function, and we
                // box it to avoid infinite-size futures.
                let children = Box::pin(scan_bus(cfg, secondary, end_bus, next_bus)).await?;

                let subordinate = next_bus.wrapping_sub(1).max(secondary);
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
                // Reserve bus numbers for SR-IOV VFs on this endpoint.
                if let Some(max_vf_bus) = probe_sriov_bus_requirement(cfg, bus, devfn).await {
                    if max_vf_bus > end_bus {
                        return Err(AssignmentError::BusExhaustion {
                            bus,
                            device: device_num,
                            function,
                        });
                    }
                    if max_vf_bus >= *next_bus {
                        *next_bus = max_vf_bus.wrapping_add(1);
                    }
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
) -> Vec<DiscoveredBar> {
    let max_bars: u8 = if is_bridge { 2 } else { 6 };
    let mut bars = Vec::new();

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

    let mut i = 0u8;
    while i < max_bars {
        let offset = HeaderType00::BAR0.0 + (i as u16) * 4;

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
            // Skip I/O BARs for now.
            i += 1;
            continue;
        }

        let encoding = BarEncodingBits::from(readback);
        let is_64bit = encoding.type_64_bit();
        let is_prefetchable = encoding.prefetchable();

        let size = if is_64bit && (i + 1) < max_bars {
            // Probe upper 32 bits.
            let upper_offset = HeaderType00::BAR0.0 + ((i + 1) as u16) * 4;
            cfg.write_u32(bus, devfn, upper_offset, !0u32).await;
            let upper_readback = cfg.read_u32(bus, devfn, upper_offset).await;

            let mask = ((upper_readback as u64) << 32) | (readback as u64 & !0xF);
            if mask == 0 {
                i += 2;
                continue;
            }
            (!mask).wrapping_add(1)
        } else {
            let mask = readback & !0xF;
            if mask == 0 {
                i += 1;
                continue;
            }
            (!(mask as u64 | (!0u64 << 32))).wrapping_add(1)
        };

        if size > 0 {
            bars.push(DiscoveredBar {
                index: i,
                size,
                is_64bit,
                is_prefetchable,
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

/// SR-IOV capability register offsets (relative to capability start).
mod sriov {
    /// Offset of the DWORD containing InitialVFs (low u16) and TotalVFs (high u16).
    pub const INITIAL_TOTAL_VFS: u16 = 0x0C;
    /// Offset of the DWORD containing VF Offset (low u16) and VF Stride (high u16).
    pub const VF_OFFSET_STRIDE: u16 = 0x14;
}

/// Probe SR-IOV capability to determine how many extra bus numbers
/// this device's VFs will need. Returns the highest bus number a VF
/// could land on, or `None` if the device has no SR-IOV capability.
async fn probe_sriov_bus_requirement(
    cfg: &mut impl PciConfigAccess,
    bus: u8,
    devfn: u8,
) -> Option<u8> {
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
                .read_u32(bus, devfn, offset + sriov::INITIAL_TOTAL_VFS)
                .await;
            let total_vfs = (vfs_dword >> 16) as u16;
            if total_vfs == 0 {
                return None;
            }

            // Read VF Offset and VF Stride.
            let offset_stride = cfg
                .read_u32(bus, devfn, offset + sriov::VF_OFFSET_STRIDE)
                .await;
            let vf_offset = offset_stride as u16;
            let vf_stride = (offset_stride >> 16) as u16;

            if vf_stride == 0 {
                return None;
            }

            // Compute the BDF of the last VF.
            // First VF routing ID = (bus << 8 | devfn) + vf_offset
            // Last VF routing ID = first + (total_vfs - 1) * vf_stride
            let pf_rid = (bus as u16) << 8 | devfn as u16;
            let last_vf_rid = pf_rid
                .wrapping_add(vf_offset)
                .wrapping_add((total_vfs - 1).wrapping_mul(vf_stride));
            let max_bus = (last_vf_rid >> 8) as u8;
            return Some(max_bus);
        }

        if next == 0 || next <= offset {
            break;
        }
        offset = next;
    }
    None
}
