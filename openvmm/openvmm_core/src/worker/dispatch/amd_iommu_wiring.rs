// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(guest_arch = "x86_64")]

//! AMD IOMMU resource setup and wiring helpers for x86_64 VMs.
//!
//! This module handles instantiating AMD IOMMU chipset devices on each
//! requested root complex.

use crate::partition::HvlitePartition;
use guestmem::GuestMemory;
use hvdef::Vtl;
use std::collections::HashMap;
use std::sync::Arc;
use vm_topology::pcie::PcieHostBridge;
use vmotherboard::ChipsetBuilder;

/// Resolved resources for a single AMD IOMMU instance, combining the
/// topology-specified RC name with the MMIO range from the layout engine.
pub(super) struct ResolvedIommuResources {
    /// Name of the PCIe root complex this IOMMU covers.
    pub rc_name: String,
    /// MMIO base address (from the memory layout allocator).
    pub mmio_base: u64,
}

/// Combines AMD IOMMU RC configs with MMIO ranges from the memory layout
/// engine into resolved per-instance resources.
pub(super) fn resolve_iommu_resources(
    root_complexes: &[openvmm_defs::config::PcieRootComplexConfig],
    mmio_ranges: &[memory_range::MemoryRange],
) -> Vec<ResolvedIommuResources> {
    root_complexes
        .iter()
        .filter(|rc| matches!(rc.iommu, Some(openvmm_defs::config::PcieIommuConfig::AmdVi)))
        .zip(mmio_ranges)
        .map(|(rc, range)| ResolvedIommuResources {
            rc_name: rc.name.clone(),
            mmio_base: range.start(),
        })
        .collect()
}

/// Result of [`setup_amd_iommu`].
pub(super) struct IommuDevicesResult {
    /// ACPI IVRS configuration for each IOMMU instance.
    pub acpi_configs: Vec<vmm_core::acpi_builder::AmdIommuAcpiConfig>,
    /// Per-RC IOMMU shared state, indexed parallel to `pcie_host_bridges`.
    /// `None` for root complexes without an AMD IOMMU.
    pub shared_states: Vec<Option<Arc<amd_iommu::IommuSharedState>>>,
}

/// Instantiate AMD IOMMU chipset devices.
///
/// Creates one `AmdIommuDevice` per root complex listed in `amd_iommu_rcs`,
/// placed as an RCiEP at device 0 on each root complex's start bus. Returns
/// ACPI configs for IVRS table generation and per-RC shared state for
/// per-device DMA/MSI wiring.
pub(super) fn setup_amd_iommu(
    resolved_resources: &[ResolvedIommuResources],
    pcie_host_bridges: &[PcieHostBridge],
    pcie_rc_name_to_idx: &HashMap<String, usize>,
    chipset_builder: &ChipsetBuilder<'_>,
    partition: &dyn HvlitePartition,
    gm: &GuestMemory,
) -> anyhow::Result<IommuDevicesResult> {
    let mut shared_states: Vec<Option<Arc<amd_iommu::IommuSharedState>>> =
        vec![None; pcie_host_bridges.len()];
    let mut acpi_configs: Vec<vmm_core::acpi_builder::AmdIommuAcpiConfig> = Vec::new();

    for res in resolved_resources {
        let rc_name = &res.rc_name;
        let rc_pos = match pcie_rc_name_to_idx.get(rc_name.as_str()) {
            Some(&i) => i,
            None => {
                let available: Vec<_> = pcie_rc_name_to_idx.keys().collect();
                anyhow::bail!(
                    "--amd-iommu references unknown root complex '{rc_name}'. \
                     Available: {available:?}"
                );
            }
        };

        if shared_states[rc_pos].is_some() {
            anyhow::bail!("duplicate AMD IOMMU for root complex '{rc_name}'");
        }

        let hb = &pcie_host_bridges[rc_pos];

        let mmio_base = res.mmio_base;
        let iommu_config = amd_iommu::AmdIommuConfig {
            mmio_base,
            pci_bdf: (0, 0, 0),
        };

        let device_name = format!("amd-iommu-{}", rc_name);
        let builder = chipset_builder
            .arc_mutex_device(device_name)
            .on_pcie_root_complex(
                vmotherboard::BusId::new(rc_name),
                0, // device 0
            );

        let iommu_bus_range = pci_core::bus_range::AssignedBusRange::new();
        iommu_bus_range.set_bus_range(hb.start_bus, hb.start_bus);
        let iommu_msi_conn = pci_core::msi::MsiConnection::new();
        let iommu_dev = builder.add(|_services| {
            amd_iommu::AmdIommuDevice::new(
                gm.clone(),
                iommu_config,
                &iommu_msi_conn.msi_target(iommu_bus_range, 0),
            )
        })?;
        if let Some(signal_msi) = partition.as_signal_msi(Vtl::Vtl0) {
            iommu_msi_conn.connect(signal_msi);
        }
        let shared = iommu_dev.lock().shared_state().clone();
        shared_states[rc_pos] = Some(shared);

        acpi_configs.push(vmm_core::acpi_builder::AmdIommuAcpiConfig {
            device_id: (hb.start_bus as u16) << 8, // device 0, function 0
            capability_offset: amd_iommu::PCI_CAP_OFFSET,
            mmio_base,
            pci_segment: hb.segment,
            ivhd_features: amd_iommu::ADVERTISED_EXT_FEAT,
            start_bus: hb.start_bus,
            end_bus: hb.end_bus,
        });
    }

    Ok(IommuDevicesResult {
        acpi_configs,
        shared_states,
    })
}
