// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(guest_arch = "x86_64")]

//! Intel VT-d resource setup and wiring helpers for x86_64 VMs.
//!
//! This module handles instantiating Intel VT-d IOMMU chipset devices on each
//! requested root complex. Unlike AMD IOMMU (which is a PCI device), VT-d is a
//! pure MMIO platform device discovered via the ACPI DMAR table.

use anyhow::Context as _;
use guestmem::GuestMemory;
use hvdef::Vtl;
use std::sync::Arc;
use vm_topology::pcie::PcieHostBridge;
use vmotherboard::ChipsetBuilder;

use crate::partition::HvlitePartition;

/// Resolved resources for a single Intel VT-d instance, combining the
/// topology-specified RC name with the MMIO range from the layout engine.
pub(super) struct ResolvedVtdResources {
    /// Index into `pcie_host_bridges` / `cfg.pcie_root_complexes`.
    pub rc_idx: usize,
    /// Name of the PCIe root complex this VT-d unit covers.
    pub rc_name: String,
    /// MMIO base address (from the memory layout allocator).
    pub mmio_base: u64,
    /// Device scopes for the DMAR DRHD: one entry per root-bus device
    /// (root ports as bridges, RCiEPs as endpoints).
    pub device_scopes: Vec<vmm_core::acpi_builder::IntelVtdDeviceScope>,
}

/// Combines Intel VT-d RC configs with MMIO ranges from the memory layout
/// engine into resolved per-instance resources.
pub(super) fn resolve_vtd_resources(
    root_complexes: &[openvmm_defs::config::PcieRootComplexConfig],
    mmio_ranges: &[memory_range::MemoryRange],
) -> Vec<ResolvedVtdResources> {
    root_complexes
        .iter()
        .enumerate()
        .filter(|(_, rc)| {
            matches!(
                rc.iommu,
                Some(openvmm_defs::config::PcieIommuConfig::IntelVtd)
            )
        })
        .zip(mmio_ranges)
        .map(|((idx, rc), range)| {
            // VT-d is a pure MMIO device — no RCiEP on bus 0. Root ports
            // start at device 0, packed 8 functions per device slot,
            // matching the GenericPcieRootComplex packing logic.
            let device_scopes = rc
                .ports
                .iter()
                .enumerate()
                .map(|(i, _)| vmm_core::acpi_builder::IntelVtdDeviceScope {
                    devfn: i as u8,
                    is_bridge: true,
                })
                .collect();

            ResolvedVtdResources {
                rc_idx: idx,
                rc_name: rc.name.clone(),
                mmio_base: range.start(),
                device_scopes,
            }
        })
        .collect()
}

/// Result of [`setup_intel_vtd`].
pub(super) struct VtdDevicesResult {
    /// ACPI DMAR configuration for each VT-d instance.
    pub acpi_configs: Vec<vmm_core::acpi_builder::IntelVtdAcpiConfig>,
    /// Per-RC VT-d shared state, indexed parallel to `pcie_host_bridges`.
    /// `None` for root complexes without an Intel VT-d unit.
    pub shared_states: Vec<Option<Arc<intel_vtd::VtdSharedState>>>,
}

/// Instantiate Intel VT-d chipset devices.
///
/// Creates one `IntelVtdDevice` per root complex listed in `vtd_rcs`.
/// Unlike AMD IOMMU, VT-d is not a PCI device — it is registered as a
/// plain chipset device with an MMIO region, discovered by the guest via
/// the DMAR ACPI table.
pub(super) fn setup_intel_vtd(
    resolved_resources: &[ResolvedVtdResources],
    pcie_host_bridges: &[PcieHostBridge],
    chipset_builder: &ChipsetBuilder<'_>,
    partition: &dyn HvlitePartition,
    gm: &GuestMemory,
) -> anyhow::Result<VtdDevicesResult> {
    let mut shared_states: Vec<Option<Arc<intel_vtd::VtdSharedState>>> =
        vec![None; pcie_host_bridges.len()];
    let mut acpi_configs: Vec<vmm_core::acpi_builder::IntelVtdAcpiConfig> = Vec::new();

    for res in resolved_resources {
        let rc_pos = res.rc_idx;
        let rc_name = &res.rc_name;

        if shared_states[rc_pos].is_some() {
            anyhow::bail!("duplicate Intel VT-d for root complex '{rc_name}'");
        }

        let hb = &pcie_host_bridges[rc_pos];

        let mmio_base = res.mmio_base;
        let vtd_config = intel_vtd::IntelVtdConfig { mmio_base };

        let device_name = format!("intel-vtd-{}", rc_name);

        // VT-d's own MSI delivery handle — for fault events and invalidation
        // completion events. These MSIs must NOT go through VT-d's own
        // interrupt remapping (VT-d calls signal_msi(None, ...) which the
        // VtdSignalMsi wrapper would drop due to missing devid).
        let signal_msi = partition
            .as_signal_msi(Vtl::Vtl0)
            .context("partition must provide MSI support for VT-d")?;

        let builder = chipset_builder.arc_mutex_device(device_name);
        let vtd_dev = builder.add(|_services| {
            let (device, _shared) =
                intel_vtd::IntelVtdDevice::new(gm.clone(), vtd_config, signal_msi);
            device
        })?;
        let shared = vtd_dev.lock().shared_state().clone();
        shared_states[rc_pos] = Some(shared);

        acpi_configs.push(vmm_core::acpi_builder::IntelVtdAcpiConfig {
            mmio_base,
            pci_segment: hb.segment,
            start_bus: hb.start_bus,
            device_scopes: res.device_scopes.clone(),
        });
    }

    Ok(VtdDevicesResult {
        acpi_configs,
        shared_states,
    })
}
