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
///
/// For accelerated SMMUs this is only a provisional value, replaced by the
/// host SMMU's OAS when a device attaches.
const DEFAULT_AUTO_OAS_BITS: u8 = 48;

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
///
/// `acpi_available` gates accelerated SMMUs, which need IORT RMR nodes to
/// reserve the host's MSI IOVA window in the guest.
pub(super) fn setup_smmu(
    root_complexes: &[openvmm_defs::config::PcieRootComplexConfig],
    resolved_smmu_resources: &[ResolvedSmmuResources],
    pcie_host_bridges: &mut [PcieHostBridge],
    chipset_builder: &ChipsetBuilder<'_>,
    gm: &GuestMemory,
    acpi_available: bool,
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
        anyhow::ensure!(
            !accel || acpi_available,
            "SMMU on root complex {}: accelerated translation requires ACPI",
            rc.name
        );

        let evtq_irq_vector = smmu.evtq_intid - *vmm_core::emuplat::gic::SPI_RANGE.start();
        let gerror_irq_vector = smmu.gerr_intid - *vmm_core::emuplat::gic::SPI_RANGE.start();
        let device_name = format!("smmu:{}", rc.name);

        // Resolve the requested OAS into a backend policy. Both policy variants
        // carry a concrete OAS: `Fixed` the requested value, `Auto` the
        // provisional default (see `DEFAULT_AUTO_OAS_BITS`) advertised until,
        // for accel, the host SMMU's OAS is adopted at device attach.
        let oas_policy = match oas {
            openvmm_defs::config::SmmuOas::Auto => smmu::SmmuOasPolicy::Auto {
                provisional: DEFAULT_AUTO_OAS_BITS,
            },
            openvmm_defs::config::SmmuOas::Fixed(bits) => {
                if !smmu::VALID_OAS_BITS.contains(&bits) {
                    anyhow::bail!(
                        "SMMU on root complex {}: OAS {bits} is not a valid SMMUv3 output \
                         address size (expected one of {:?})",
                        rc.name,
                        smmu::VALID_OAS_BITS
                    );
                }
                smmu::SmmuOasPolicy::Fixed(bits)
            }
        };

        let smmu_config = smmu::SmmuConfig {
            sidsize: 16,
            oas_policy,
            accel,
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
        // When the SMMU is in accel mode (iommufd nested), the L1
        // kernel's MSI reserved IOVA window must be identity-mapped in
        // the L2 guest's S1 page tables. The window is 128MB–129MB
        // (0x800_0000–0x810_0000), which is the default ARM IOMMU MSI
        // reserved region.
        let reserved_iova_ranges = if accel {
            // These reserved IOVA ranges become IORT RMR entries. Mark the
            // root complex so the SSDT emits a PCI Firmware _DSM (function 5,
            // preserve boot config); Linux skips RMR entries for root
            // complexes without this flag.
            pcie_host_bridges[rc_pos].preserve_boot_config = true;
            vec![memory_range::MemoryRange::new(0x800_0000..0x810_0000)]
        } else {
            Vec::new()
        };

        configs.push(vmm_core::acpi_builder::AcpiSmmuConfig {
            rc_index: pcie_host_bridges[rc_pos].index,
            segment: pcie_host_bridges[rc_pos].segment,
            base: smmu.base,
            event_gsiv: smmu.evtq_intid,
            gerr_gsiv: smmu.gerr_intid,
            reserved_iova_ranges,
        });
    }

    Ok(SmmuDevicesResult {
        shared_states,
        configs,
    })
}
