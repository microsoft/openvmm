// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Phase 1: PCI bus enumeration and BAR size probing.

use crate::AssignmentError;
use crate::AssignmentParams;
use crate::PciConfigAccess;
use pci_core::spec::caps::CapabilityId;
use pci_core::spec::caps::EXT_CAP_START;
use pci_core::spec::caps::ExtendedCapabilityId;
use pci_core::spec::caps::ari::ARI_CAPABILITY_NEXT_FUNCTION_SHIFT;
use pci_core::spec::caps::ari::AriExtendedCapabilityHeader;
use pci_core::spec::caps::pci_express::DeviceCapabilities2;
use pci_core::spec::caps::pci_express::DeviceControl2;
use pci_core::spec::caps::pci_express::PciExpressCapabilityHeader;
use pci_core::spec::caps::sriov::SRIOV_CONTROL_ARI_CAPABLE_HIERARCHY;
use pci_core::spec::caps::sriov::SriovExtendedCapabilityHeader;
use pci_core::spec::cfg_space::BarEncodingBits;
use pci_core::spec::cfg_space::BistHeader;
use pci_core::spec::cfg_space::Command;
use pci_core::spec::cfg_space::CommonHeader;
use pci_core::spec::cfg_space::HeaderType00;
use pci_core::spec::cfg_space::HeaderType01;

/// Status register bit 4: Capabilities List present.
const STATUS_CAPABILITIES_LIST: u16 = 1 << 4;

/// A discovered PCI device or bridge with probed BAR sizes.
#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    pub bus: u8,
    pub device: u8,
    /// Function Number.
    ///
    /// For a traditional Function this is 0..7. For the Extended Functions of
    /// an ARI Device the Device Number field is eliminated and this holds the
    /// full 8-bit ARI Function Number (which may exceed 7); in that case
    /// [`device`](Self::device) is 0, so the config-space devfn byte is still
    /// `crate::devfn(device, function)`.
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
        None,
        params.end_bus,
        &mut next_bus,
        params.preserve_bars,
    )
    .await
}

/// Scan a single bus (non-recursive helper that does DFS via inner calls).
/// This is called for secondary buses behind bridges. It's not async-recursive
/// itself because we use the pattern of scanning children inline.
///
/// `parent_port` is the `(bus, devfn)` of the bridge (Root Port or Switch
/// Downstream Port) immediately above this bus, or `None` for the host
/// bridge's root bus. It is used to enable ARI Forwarding in the downstream
/// port above an ARI Device (see [`enable_ari_forwarding`]).
async fn scan_bus(
    cfg: &mut impl PciConfigAccess,
    bus: u8,
    parent_port: Option<(u8, u8)>,
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
        let header_type0 = bist.header_type();
        let multi_function = bist.multi_function();

        // An ARI Device is identified by an ARI Extended Capability structure
        // in Function 0 (the head of the Extended-Function linked list). This
        // is reachable without ARI Forwarding since it lives at devfn 0.
        //
        // ARI eliminates the Device Number field, so an ARI Device always
        // occupies Device 0 on its bus. Only treat Device 0 as a possible ARI
        // Device; this keeps `crate::devfn(device, function)` correct for
        // Extended Function Numbers > 7 (since `device` is 0).
        let ari_cap_f0 = if device_num == 0 {
            find_ext_cap(
                cfg,
                bus,
                crate::devfn(device_num, 0),
                ExtendedCapabilityId::ARI,
            )
            .await
        } else {
            None
        };

        // If this is an ARI Device, enable ARI Forwarding in the downstream
        // port above it *before* enumerating its Functions, so that Extended
        // Functions (Function Numbers > 7) become reachable via Type 0
        // Configuration Requests. See PCIe Base 7.0 §6.13.
        let mut ari_forwarding_enabled = false;
        if ari_cap_f0.is_some() {
            if let Some((pbus, pdevfn)) = parent_port {
                ari_forwarding_enabled = enable_ari_forwarding(cfg, pbus, pdevfn).await;
            }
        }

        // Build the list of Function Numbers to enumerate. For an ARI Device
        // this walks the ARI Next-Function-Number linked list (§7.8.8.2),
        // which may include sparse Function Numbers greater than 7. Otherwise
        // scan the traditional Function Numbers 0..7.
        let func_nums: Vec<u8> = if ari_cap_f0.is_some() {
            ari_function_list(cfg, bus).await
        } else if multi_function {
            let mut v = Vec::new();
            for f in 0..8u8 {
                if f == 0
                    || cfg
                        .read_u32(
                            bus,
                            crate::devfn(device_num, f),
                            CommonHeader::DEVICE_VENDOR.0,
                        )
                        .await
                        != !0u32
                {
                    v.push(f);
                }
            }
            v
        } else {
            vec![0]
        };

        // Detect the header type and SR-IOV / SIOV capabilities of each
        // Function. This is done before probing so that ARI Forwarding and
        // ARI Capable Hierarchy can be configured before First VF Offset /
        // VF Stride are read (their values may depend on ARI Capable
        // Hierarchy — see §9.2.1.2).
        struct FuncInfo {
            func_num: u8,
            devfn: u8,
            is_bridge: bool,
            sriov_off: Option<u16>,
            is_siov: bool,
        }
        let mut funcs = Vec::new();
        for &f in &func_nums {
            // For an ARI Device the Device Number is eliminated, so the devfn
            // byte is the raw Function Number. Since ARI Devices reside at
            // Device 0, `devfn(0, f)` equals `f` for any `f`.
            let devfn = crate::devfn(device_num, f);
            let is_bridge = if f == 0 {
                header_type0 == 1
            } else {
                let bh = cfg.read_u32(bus, devfn, HeaderType00::BIST_HEADER.0).await;
                BistHeader::from(bh).header_type() == 1
            };
            // SR-IOV / SIOV Extended Capabilities only appear on endpoint
            // (Type 0) Functions.
            let (sriov_off, is_siov) = if is_bridge {
                (None, false)
            } else {
                let sriov_off = find_ext_cap(cfg, bus, devfn, ExtendedCapabilityId::SRIOV).await;
                let is_siov = find_ext_cap(cfg, bus, devfn, ExtendedCapabilityId::SIOV)
                    .await
                    .is_some();
                (sriov_off, is_siov)
            };
            funcs.push(FuncInfo {
                func_num: f,
                devfn,
                is_bridge,
                sriov_off,
                is_siov,
            });
        }

        let has_sriov = funcs.iter().any(|f| f.sriov_off.is_some());
        let has_siov = funcs.iter().any(|f| f.is_siov);

        // Enable ARI Forwarding in the downstream port above this device when
        // it exposes SR-IOV or SIOV Functions (and it was not already enabled
        // for an ARI Device above). SR-IOV VFs and SIOV SDIs may consume
        // Function Numbers greater than 7 when ARI Capable Hierarchy is set.
        if !ari_forwarding_enabled && (has_sriov || has_siov) {
            if let Some((pbus, pdevfn)) = parent_port {
                ari_forwarding_enabled = enable_ari_forwarding(cfg, pbus, pdevfn).await;
            }
        }

        // Set ARI Capable Hierarchy in the lowest-numbered SR-IOV PF, but only
        // when ARI Forwarding was actually enabled in the downstream port
        // above: software should set this bit to *match* the port's ARI
        // Forwarding Enable (§9.4.3.3.5). This bit is present only in the
        // lowest-numbered PF and affects all PFs of the device; it must be set
        // before First VF Offset / VF Stride are read below.
        if ari_forwarding_enabled {
            if let Some(pf) = funcs
                .iter()
                .filter(|f| f.sriov_off.is_some())
                .min_by_key(|f| f.func_num)
            {
                set_ari_capable_hierarchy(cfg, bus, pf.devfn, pf.sriov_off.unwrap()).await;
            }
        }

        if ari_forwarding_enabled {
            tracing::debug!(
                bus,
                device = device_num,
                is_ari_device = ari_cap_f0.is_some(),
                has_sriov,
                has_siov,
                "enabled ARI forwarding in downstream port"
            );
        }

        // Probe each Function and build its DiscoveredDevice.
        for info in funcs {
            let FuncInfo {
                func_num,
                devfn,
                is_bridge,
                sriov_off,
                is_siov: _,
            } = info;

            let bars = probe_bars(cfg, bus, devfn, is_bridge, preserve_bars).await;

            let mut dev = DiscoveredDevice {
                bus,
                device: device_num,
                function: func_num,
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
                        function: func_num,
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
                // box it to avoid infinite-size futures. This bridge is the
                // downstream port above the secondary bus.
                let children = Box::pin(scan_bus(
                    cfg,
                    secondary,
                    Some((bus, devfn)),
                    end_bus,
                    next_bus,
                    preserve_bars,
                ))
                .await?;

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
                    function = func_num,
                    secondary,
                    subordinate,
                    "bridge enumerated"
                );
            } else {
                // Probe SR-IOV capability for bus reservation and VF BAR sizes.
                // First VF Offset / VF Stride now reflect ARI Capable
                // Hierarchy, which was configured above.
                if let Some(sriov_result) =
                    probe_sriov(cfg, bus, devfn, sriov_off, preserve_bars).await
                {
                    let max_vf_bus = sriov_result.max_vf_bus;
                    if max_vf_bus > end_bus as u16 {
                        return Err(AssignmentError::BusExhaustion {
                            bus,
                            device: device_num,
                            function: func_num,
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
                                function: func_num,
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
                    function = func_num,
                    bar_count = dev.bars.len(),
                    "endpoint enumerated"
                );
            }

            devices.push(dev);
        }

        // When ARI Forwarding is enabled in the downstream port above, that
        // port stops enforcing the Device Number == 0 restriction when
        // converting Type 1 Configuration Requests to Type 0 (PCIe Base 7.0
        // §6.13). The ARI Device below then occupies the entire Function-Number
        // space of Device 0, and traditional Device Numbers 1..31 alias to ARI
        // Function Numbers >= 8 (already enumerated via the linked-list walk
        // above). Stop scanning further Device Numbers to avoid re-enumerating
        // those Extended Functions.
        //
        // This aliasing only occurs once ARI Forwarding is enabled. If it was
        // not (e.g., the root bus has no upstream port, or the port does not
        // support ARI Forwarding), Device Numbers 1..31 are either absent
        // (point-to-point link below a downstream port) or real, independent
        // devices (root/conventional bus) that must still be scanned.
        if ari_forwarding_enabled {
            break;
        }
    }

    Ok(devices)
}

/// Read a single byte from config space via a 32-bit aligned read.
async fn read_config_u8(cfg: &mut impl PciConfigAccess, bus: u8, devfn: u8, offset: u16) -> u8 {
    let dword = cfg.read_u32(bus, devfn, offset & !0x3).await;
    (dword >> ((offset & 0x3) * 8)) as u8
}

/// Walk the standard (PCI-compatible) Capabilities List to find the PCI
/// Express Capability (ID 0x10). Returns its config-space offset, or `None`
/// if the Function has no Capabilities List or no PCI Express Capability.
async fn find_pcie_cap(cfg: &mut impl PciConfigAccess, bus: u8, devfn: u8) -> Option<u16> {
    let status = (cfg
        .read_u32(bus, devfn, CommonHeader::STATUS_COMMAND.0)
        .await
        >> 16) as u16;
    if status & STATUS_CAPABILITIES_LIST == 0 {
        return None;
    }
    let mut ptr = read_config_u8(cfg, bus, devfn, CommonHeader::RESERVED_CAP_PTR.0).await as u16;
    // Capabilities live in the range 0x40..0x100; bound the walk to that many
    // entries to guard against malformed loops.
    for _ in 0..48 {
        if ptr < 0x40 || ptr == 0xFF {
            break;
        }
        let cap_id = read_config_u8(cfg, bus, devfn, ptr).await;
        if cap_id == CapabilityId::PCI_EXPRESS.0 {
            return Some(ptr);
        }
        ptr = read_config_u8(cfg, bus, devfn, ptr + 1).await as u16;
    }
    None
}

/// Walk the PCIe Extended Capabilities list (starting at 0x100) to find the
/// capability with the given ID. Returns its config-space offset, or `None`.
async fn find_ext_cap(
    cfg: &mut impl PciConfigAccess,
    bus: u8,
    devfn: u8,
    cap_id: ExtendedCapabilityId,
) -> Option<u16> {
    let mut offset = EXT_CAP_START;
    loop {
        if offset < EXT_CAP_START || offset & 0x3 != 0 {
            break;
        }
        let header = cfg.read_u32(bus, devfn, offset).await;
        if header == 0 || header == !0u32 {
            break;
        }
        let id = (header & 0xFFFF) as u16;
        let next = ((header >> 20) & 0xFFC) as u16;
        if id == cap_id.0 {
            return Some(offset);
        }
        if next == 0 || next <= offset {
            break;
        }
        offset = next;
    }
    None
}

/// Enable ARI Forwarding in a downstream port (Root Port or Switch Downstream
/// Port), if the port supports it. Setting the ARI Forwarding Enable bit in
/// the port's Device Control 2 register lets Configuration Requests reach
/// Extended Functions (Function Numbers > 7) in the ARI Device below. Returns
/// `true` if ARI Forwarding is enabled (either newly or already).
///
/// See PCIe Base 7.0 §6.13, §7.5.3.15 (ARI Forwarding Supported), and
/// §7.5.3.16 (ARI Forwarding Enable).
async fn enable_ari_forwarding(cfg: &mut impl PciConfigAccess, bus: u8, devfn: u8) -> bool {
    let Some(pcie) = find_pcie_cap(cfg, bus, devfn).await else {
        return false;
    };
    let dev_caps2 = DeviceCapabilities2::from(
        cfg.read_u32(
            bus,
            devfn,
            pcie + PciExpressCapabilityHeader::DEVICE_CAPS_2.0,
        )
        .await,
    );
    if !dev_caps2.ari_forwarding_supported() {
        return false;
    }
    let ctl_sts_off = pcie + PciExpressCapabilityHeader::DEVICE_CTL_STS_2.0;
    let ctl_sts = cfg.read_u32(bus, devfn, ctl_sts_off).await;
    // Device Control 2 is the low 16 bits; Device Status 2 the high 16 bits.
    let control = DeviceControl2::from(ctl_sts as u16);
    if control.ari_forwarding_enable() {
        return true;
    }
    let control = control.with_ari_forwarding_enable(true);
    // Preserve the high 16 bits (Device Status 2, reserved) as read.
    let new = (ctl_sts & 0xFFFF_0000) | control.into_bits() as u32;
    cfg.write_u32(bus, devfn, ctl_sts_off, new).await;
    true
}

/// Set the ARI Capable Hierarchy bit in a PF's SR-IOV Control register. This
/// hints to the device that ARI has been enabled in the downstream port above
/// it, allowing VFs to be packed into Function Numbers greater than 7 to
/// conserve Bus Numbers. See PCIe Base 7.0 §9.4.3.3.5.
async fn set_ari_capable_hierarchy(
    cfg: &mut impl PciConfigAccess,
    bus: u8,
    devfn: u8,
    sriov_offset: u16,
) {
    let off = sriov_offset + SriovExtendedCapabilityHeader::CONTROL_STATUS.0;
    let control = cfg.read_u32(bus, devfn, off).await as u16;
    if control & SRIOV_CONTROL_ARI_CAPABLE_HIERARCHY != 0 {
        return;
    }
    // Write only the 16-bit SR-IOV Control field, leaving the SR-IOV Status
    // field (high 16 bits, which contains W1C bits) as zero — a no-op.
    let new = (control | SRIOV_CONTROL_ARI_CAPABLE_HIERARCHY) as u32;
    cfg.write_u32(bus, devfn, off, new).await;
}

/// Enumerate an ARI Device's Function Numbers by walking the ARI Capability
/// register's Next-Function-Number linked list. Function 0 is the head; each
/// Function's ARI Capability register (bits 15:8) points to the next higher
/// Function Number, or 0 to terminate. Function Numbers may be sparse and
/// greater than 7. See PCIe Base 7.0 §6.13, §7.8.8.2.
///
/// Reaching Functions with Function Number > 7 requires ARI Forwarding to
/// already be enabled in the downstream port above the device.
async fn ari_function_list(cfg: &mut impl PciConfigAccess, bus: u8) -> Vec<u8> {
    let mut funcs = Vec::new();
    let mut func = 0u8;
    // At most 256 Functions; bound the walk to guard against malformed lists.
    for _ in 0..256 {
        // For an ARI Device the devfn byte is the raw Function Number.
        let devfn = func;
        if cfg
            .read_u32(bus, devfn, CommonHeader::DEVICE_VENDOR.0)
            .await
            == !0u32
        {
            break;
        }
        funcs.push(func);
        let Some(ari_off) = find_ext_cap(cfg, bus, devfn, ExtendedCapabilityId::ARI).await else {
            break;
        };
        let cap = cfg
            .read_u32(
                bus,
                devfn,
                ari_off + AriExtendedCapabilityHeader::CAPABILITY_CONTROL.0,
            )
            .await;
        let next = (cap >> ARI_CAPABILITY_NEXT_FUNCTION_SHIFT) as u8;
        // A Next Function Number of 0 terminates the list; it must strictly
        // increase, so anything not greater than the current Function is
        // treated as the end to avoid looping.
        if next <= func {
            break;
        }
        func = next;
    }
    funcs
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

/// Probe an SR-IOV capability (already located at `sriov_offset`) to determine
/// bus requirements and VF BAR sizes. Returns `None` if the PF has no VFs.
///
/// This must be called *after* ARI Capable Hierarchy has been configured,
/// since First VF Offset / VF Stride may depend on it (§9.2.1.2).
async fn probe_sriov(
    cfg: &mut impl PciConfigAccess,
    bus: u8,
    devfn: u8,
    sriov_offset: Option<u16>,
    preserve_bars: bool,
) -> Option<SriovProbeResult> {
    let offset = sriov_offset?;

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

    Some(SriovProbeResult {
        cap_offset: offset,
        max_vf_bus,
        total_vfs,
        vf_bars,
    })
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
