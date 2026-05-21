// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMU resource resolution and wiring helpers for aarch64 VMs.
//!
//! This module handles combining SMMU MMIO ranges (from the memory layout
//! allocator) with SPI assignments (from the SPI allocator) into resolved
//! resources, instantiating SMMU chipset devices, and building the lookup
//! maps needed for per-device wiring.

use chipset_device_resources::IRQ_LINE_SET;
use closeable_mutex::CloseableMutex;
use guestmem::GuestMemory;
use pcie::root::GenericPcieRootComplex;
use std::collections::HashMap;
use std::sync::Arc;
use vm_topology::pcie::PcieHostBridge;
use vmotherboard::ChipsetBuilder;

/// Resolved resources for a single SMMUv3 instance, combining MMIO and SPI
/// allocations.
pub(super) struct ResolvedSmmuResources {
    /// MMIO base address (from the memory layout allocator).
    pub base: u64,
    /// GIC INTID for the event queue interrupt (from the SPI allocator).
    pub evtq_intid: u32,
    /// GIC INTID for the global error interrupt (from the SPI allocator).
    pub gerr_intid: u32,
}

/// Combines SMMU MMIO ranges from the memory layout with SPI assignments from
/// the SPI layout into resolved resources.
pub(super) fn resolve_smmu_resources(
    smmu_ranges: &[memory_range::MemoryRange],
    spi_layout: &crate::worker::spi_layout::ResolvedSpiLayout,
) -> Vec<ResolvedSmmuResources> {
    smmu_ranges
        .iter()
        .zip(&spi_layout.smmu)
        .map(|(range, spis)| ResolvedSmmuResources {
            base: range.start(),
            evtq_intid: spis.evtq_intid,
            gerr_intid: spis.gerr_intid,
        })
        .collect()
}

/// Lookup maps for SMMU-covered PCIe ports, used during device wiring.
pub(super) struct SmmuPortMaps {
    /// Maps port names to their SMMU shared state (for per-device wrapping).
    pub port_map: HashMap<Arc<str>, Arc<smmu::SmmuSharedState>>,
}

/// Result of [`setup_smmu`].
pub(super) struct SmmuDevicesResult {
    /// Per-RC SMMU shared state, indexed parallel to `pcie_host_bridges`.
    /// `None` for root complexes without an SMMU.
    pub shared_states: Vec<Option<Arc<smmu::SmmuSharedState>>>,
    /// ACPI IORT configuration for each SMMU instance.
    pub configs: Vec<vmm_core::acpi_builder::AcpiSmmuConfig>,
    /// Port-level lookup maps for per-device wiring and VFIO validation.
    pub port_maps: SmmuPortMaps,
}

/// Extract SMMU instance configs from the processor topology, instantiate
/// SMMU chipset devices, and build the port-level lookup maps.
///
/// This is the single entry point for all SMMU setup in dispatch. It
/// extracts `SmmuInstanceConfig`s from the arch topology, creates one
/// `SmmuDevice` per instance on the chipset builder, and builds the
/// port-name maps needed for per-device wiring.
pub(super) fn setup_smmu(
    processor_topology_arch: Option<&openvmm_defs::config::ArchTopologyConfig>,
    resolved_smmu_resources: &[ResolvedSmmuResources],
    pcie_rc_name_to_idx: &HashMap<String, usize>,
    pcie_host_bridges: &[PcieHostBridge],
    pcie_root_complexes: &[Arc<CloseableMutex<GenericPcieRootComplex>>],
    chipset_builder: &ChipsetBuilder<'_>,
    gm: &GuestMemory,
) -> anyhow::Result<SmmuDevicesResult> {
    // Extract SMMU instance configs from the arch topology.
    let smmu_instances: &[openvmm_defs::config::SmmuInstanceConfig] = match processor_topology_arch
    {
        Some(openvmm_defs::config::ArchTopologyConfig::Aarch64(
            openvmm_defs::config::Aarch64TopologyConfig { smmu, .. },
        )) => smmu.as_slice(),
        _ => &[],
    };

    // Instantiate SMMU chipset devices.
    let mut shared_states: Vec<Option<Arc<smmu::SmmuSharedState>>> =
        vec![None; pcie_host_bridges.len()];
    let mut configs = Vec::new();

    for (idx, inst) in smmu_instances.iter().enumerate() {
        let rc_pos = match pcie_rc_name_to_idx.get(&inst.rc_name) {
            Some(&i) => i,
            None => {
                anyhow::bail!(
                    "SMMU instance references unknown root complex {:?}",
                    inst.rc_name
                );
            }
        };
        if shared_states[rc_pos].is_some() {
            anyhow::bail!(
                "duplicate SMMU instance for root complex {:?}",
                inst.rc_name
            );
        }

        let smmu = &resolved_smmu_resources[idx];
        let evtq_irq_vector = smmu.evtq_intid - *vmm_core::emuplat::gic::SPI_RANGE.start();
        let gerror_irq_vector = smmu.gerr_intid - *vmm_core::emuplat::gic::SPI_RANGE.start();
        let device_name = format!("smmu:{}", inst.rc_name);
        let smmu_config = smmu::SmmuConfig {
            sidsize: 16,
            oas: 44,
        };
        let smmu_device =
            chipset_builder
                .arc_mutex_device(device_name.as_str())
                .add(|services| {
                    let evtq_irq = services.new_line(IRQ_LINE_SET, "evtq", evtq_irq_vector);
                    let gerror_irq = services.new_line(IRQ_LINE_SET, "gerror", gerror_irq_vector);
                    smmu::SmmuDevice::new(
                        smmu.base,
                        gm.clone(),
                        &smmu_config,
                        Some(evtq_irq),
                        Some(gerror_irq),
                    )
                })?;

        shared_states[rc_pos] = Some(smmu_device.lock().shared_state().clone());
        configs.push(vmm_core::acpi_builder::AcpiSmmuConfig {
            rc_index: pcie_host_bridges[rc_pos].index,
            segment: pcie_host_bridges[rc_pos].segment,
            base: smmu.base,
            event_gsiv: smmu.evtq_intid,
            gerr_gsiv: smmu.gerr_intid,
        });
    }

    // Build port-level lookup maps from the per-RC shared state.
    let port_maps = build_smmu_port_maps(&shared_states, pcie_root_complexes);

    Ok(SmmuDevicesResult {
        shared_states,
        configs,
        port_maps,
    })
}

/// Builds the port-level SMMU lookup maps from per-RC shared state.
///
/// `smmu_shared_states` is indexed parallel to `pcie_root_complexes`,
/// with `None` for root complexes that have no SMMU.
fn build_smmu_port_maps(
    smmu_shared_states: &[Option<Arc<smmu::SmmuSharedState>>],
    pcie_root_complexes: &[Arc<CloseableMutex<GenericPcieRootComplex>>],
) -> SmmuPortMaps {
    let mut port_map: HashMap<Arc<str>, Arc<smmu::SmmuSharedState>> = HashMap::new();

    for (shared, rc) in smmu_shared_states.iter().zip(pcie_root_complexes.iter()) {
        let ports = rc.lock().downstream_ports();
        if let Some(shared) = shared {
            for dpi in ports {
                port_map.insert(dpi.name, shared.clone());
            }
        }
    }

    SmmuPortMaps { port_map }
}
