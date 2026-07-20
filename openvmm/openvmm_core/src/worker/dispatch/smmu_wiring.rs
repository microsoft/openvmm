// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(guest_arch = "aarch64")]

//! SMMU resource resolution and wiring helpers for aarch64 VMs.
//!
//! This module handles combining SMMU MMIO ranges (from the memory layout
//! allocator) with SPI assignments (from the SPI allocator) into resolved
//! resources and instantiating SMMU chipset devices.

use chipset_device_resources::IRQ_LINE_SET;
use guestmem::GuestMemory;
use std::sync::Arc;
use vm_topology::pcie::PcieHostBridge;
use vmotherboard::ChipsetBuilder;

/// Default advertised OAS (in bits) for an `oas=auto` SMMU.
///
/// This is a fixed sizing policy, not a computed maximum: rather than sizing
/// the advertised OAS to the guest memory layout (or to the host's supported
/// IPA width, which on aarch64/KVM can be up to 52 bits), an `auto` SMMU
/// advertises a constant 48 bits. This matches the fixed-OAS approach taken by
/// Hyper-V's emulated SMMU.
///
/// The memory-layout allocator packs high MMIO compactly bottom-up just above
/// guest RAM, so for typical configurations every translatable address sits
/// far below 48 bits (256 TiB). This is not a hard guarantee, though: a large
/// enough RAM size, or an explicitly pinned high MMIO/ECAM base, can place
/// addresses above 256 TiB (up to the host IPA width). Such a configuration
/// must pass an explicit `oas=` (e.g. `oas=52`) rather than relying on `auto`.
const DEFAULT_AUTO_OAS_BITS: u8 = 48;

/// Valid SMMUv3 output address sizes (IDR5.OAS encodings), in bits.
const VALID_OAS_BITS: [u8; 7] = [32, 36, 40, 42, 44, 48, 52];

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

/// Result of [`setup_smmu`].
pub(super) struct SmmuDevicesResult {
    /// Per-RC SMMU shared state, indexed parallel to `pcie_host_bridges`.
    /// `None` for root complexes without an SMMU.
    pub shared_states: Vec<Option<Arc<smmu::SmmuSharedState>>>,
    /// ACPI IORT configuration for each SMMU instance.
    pub configs: Vec<vmm_core::acpi_builder::AcpiSmmuConfig>,
}

/// Instantiate SMMU chipset devices for root complexes that have SMMU
/// configured.
///
/// This is the single entry point for all SMMU setup in dispatch. It
/// iterates root complex configs, creates one `SmmuDevice` per RC with
/// `iommu: Some(Smmu)`, and wires up interrupts.
pub(super) fn setup_smmu(
    root_complexes: &[openvmm_defs::config::PcieRootComplexConfig],
    resolved_smmu_resources: &[ResolvedSmmuResources],
    pcie_host_bridges: &[PcieHostBridge],
    chipset_builder: &ChipsetBuilder<'_>,
    gm: &GuestMemory,
) -> anyhow::Result<SmmuDevicesResult> {
    // Instantiate SMMU chipset devices.
    let mut shared_states: Vec<Option<Arc<smmu::SmmuSharedState>>> =
        vec![None; pcie_host_bridges.len()];
    let mut configs = Vec::new();

    // Iterate RCs with SMMU enabled, zipping with resolved MMIO+SPI resources.
    let smmu_rcs = root_complexes
        .iter()
        .enumerate()
        .filter_map(|(rc_pos, rc)| match &rc.iommu {
            Some(openvmm_defs::config::PcieIommuConfig::Smmu { accel, oas }) => {
                Some((rc_pos, rc, *accel, *oas))
            }
            _ => None,
        });

    for ((rc_pos, rc, accel, oas), smmu) in smmu_rcs.zip(resolved_smmu_resources) {
        // Accelerated (iommufd-nested) SMMUs are not yet wired up. Reject the
        // request explicitly rather than silently falling back to emulated
        // translation.
        anyhow::ensure!(
            !accel,
            "SMMU on root complex {}: accelerated translation is not yet supported",
            rc.name
        );

        // Resolve the requested OAS into a concrete advertised value.
        let oas_bits = match oas {
            openvmm_defs::config::SmmuOas::Auto => DEFAULT_AUTO_OAS_BITS,
            openvmm_defs::config::SmmuOas::Fixed(bits) => {
                anyhow::ensure!(
                    VALID_OAS_BITS.contains(&bits),
                    "SMMU on root complex {}: OAS {bits} is not a valid SMMUv3 output \
                     address size (expected one of {:?})",
                    rc.name,
                    VALID_OAS_BITS
                );
                bits
            }
        };

        let evtq_irq_vector = smmu.evtq_intid - *vmm_core::emuplat::gic::SPI_RANGE.start();
        let gerror_irq_vector = smmu.gerr_intid - *vmm_core::emuplat::gic::SPI_RANGE.start();
        let device_name = format!("smmu:{}", rc.name);
        let smmu_config = smmu::SmmuConfig {
            sidsize: 16,
            oas: oas_bits,
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

    Ok(SmmuDevicesResult {
        shared_states,
        configs,
    })
}
