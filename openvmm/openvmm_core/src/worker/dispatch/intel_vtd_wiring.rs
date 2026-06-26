// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(guest_arch = "x86_64")]

//! Intel VT-d resource setup and wiring helpers for x86_64 VMs.
//!
//! This module handles instantiating Intel VT-d IOMMU chipset devices on each
//! requested root complex. Unlike AMD IOMMU (which is a PCI device), VT-d is a
//! pure MMIO platform device discovered via the ACPI DMAR table.

use super::ioapic_iommu_wiring::IoapicIommuSelection;
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
///
/// Intel VT-d forces global interrupt remapping (x2APIC), under which Linux
/// gives any device not covered by a DRHD a NULL MSI domain and its MSI
/// allocation fails. Each VT-d unit covers only its own root complex, so
/// reject mixing VT-d and non-VT-d root complexes.
pub(super) fn resolve_vtd_resources(
    root_complexes: &[openvmm_defs::config::PcieRootComplexConfig],
    mmio_ranges: &[memory_range::MemoryRange],
) -> anyhow::Result<Vec<ResolvedVtdResources>> {
    for rc in root_complexes {
        if !matches!(
            rc.iommu,
            Some(openvmm_defs::config::PcieIommuConfig::IntelVtd)
        ) {
            anyhow::bail!(
                "root complex '{}' has no Intel VT-d unit; with Intel \
                 VT-d, every root complex must have one",
                rc.name,
            );
        }
    }

    Ok(root_complexes
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
        .collect())
}

/// Result of [`setup_intel_vtd`].
pub(super) struct VtdDevicesResult {
    /// ACPI DMAR configuration for each VT-d instance.
    pub acpi_configs: Vec<vmm_core::acpi_builder::IntelVtdAcpiConfig>,
    /// Per-RC VT-d shared state, indexed parallel to `pcie_host_bridges`.
    /// `None` for root complexes without an Intel VT-d unit.
    pub shared_states: Vec<Option<Arc<intel_vtd::VtdSharedState>>>,
    /// The VT-d unit that covers the southbridge IOAPIC (segment 0, bus 0),
    /// if any. Used to wire IOAPIC interrupt remapping and publish the DMAR
    /// IOAPIC device scope.
    pub ioapic_iommu: Option<IoapicIommuSelection>,
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
    let mut ioapic_iommu: Option<IoapicIommuSelection> = None;

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
        shared_states[rc_pos] = Some(shared.clone());

        let ioapic_rid = if hb.segment == 0 && hb.start_bus == 0 {
            Some(
                ((hb.start_bus as u16) << 8)
                    | super::ioapic_iommu_wiring::IOAPIC_PHANTOM_DEVFN as u16,
            )
        } else {
            None
        };

        acpi_configs.push(vmm_core::acpi_builder::IntelVtdAcpiConfig {
            mmio_base,
            pci_segment: hb.segment,
            start_bus: hb.start_bus,
            device_scopes: res.device_scopes.clone(),
        });

        if let Some(ioapic_rid) = ioapic_rid {
            ioapic_iommu = Some(IoapicIommuSelection {
                remapper: shared,
                ioapic_rid,
            });
        }
    }

    if ioapic_iommu.is_none() && !acpi_configs.is_empty() {
        tracing::warn!(
            "no Intel VT-d unit covers segment 0 bus 0; IOAPIC interrupt remapping disabled"
        );
    }

    Ok(VtdDevicesResult {
        acpi_configs,
        shared_states,
        ioapic_iommu,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use memory_range::MemoryRange;
    use openvmm_defs::config::PcieIommuConfig;
    use openvmm_defs::config::PcieMmioRangeConfig;
    use openvmm_defs::config::PcieRootComplexConfig;

    fn rc(name: &str, segment: u16, iommu: Option<PcieIommuConfig>) -> PcieRootComplexConfig {
        PcieRootComplexConfig {
            index: 0,
            name: name.to_string(),
            segment,
            start_bus: 0,
            end_bus: 0,
            low_mmio: PcieMmioRangeConfig::Dynamic { size: 0 },
            high_mmio: PcieMmioRangeConfig::Dynamic { size: 0 },
            ports: Vec::new(),
            cxl: None,
            iommu,
            vnode: None,
            preserve_bars: false,
        }
    }

    #[test]
    fn rejects_mixed_vtd_topology() {
        // A root complex without its own VT-d unit is unreachable by interrupt
        // remapping once VT-d forces global x2APIC, so the config is rejected.
        let rcs = [
            rc("s0rc0", 0, Some(PcieIommuConfig::IntelVtd)),
            rc("s1rc0", 1, None),
        ];
        assert!(resolve_vtd_resources(&rcs, &[]).is_err());
    }

    #[test]
    fn accepts_vtd_on_every_rc() {
        let rcs = [
            rc("s0rc0", 0, Some(PcieIommuConfig::IntelVtd)),
            rc("s1rc0", 1, Some(PcieIommuConfig::IntelVtd)),
        ];
        let ranges = [
            MemoryRange::new(0..0x1000),
            MemoryRange::new(0x1000..0x2000),
        ];
        assert_eq!(resolve_vtd_resources(&rcs, &ranges).unwrap().len(), 2);
    }
}
