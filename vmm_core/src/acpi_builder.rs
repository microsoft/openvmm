// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Construct ACPI tables for a concrete VM topology

// TODO: continue to remove these hardcoded deps
use acpi::cedt::Cedt;
use acpi::dsdt;
use acpi::ssdt::Ssdt;
use acpi_spec::madt::InterruptPolarity;
use acpi_spec::madt::InterruptTriggerMode;
use cache_topology::CacheTopology;
use chipset::ioapic;
use chipset::psp;
use inspect::Inspect;
use std::collections::BTreeMap;
use thiserror::Error;
use vm_topology::memory::MemoryLayout;
use vm_topology::pcie::PcieHostBridge;
use vm_topology::processor::ArchTopology;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::aarch64::Aarch64Topology;
use vm_topology::processor::x86::X86Topology;
use x86defs::apic::APIC_BASE_ADDRESS;
use zerocopy::IntoBytes;

/// Configuration for the SMMUv3 ACPI IORT node.
#[derive(Debug, Clone)]
pub struct AcpiSmmuConfig {
    /// Index of the root complex this SMMU covers (matches
    /// `PcieHostBridge.index`). Used to route each RC's IORT ID mapping
    /// to its specific SMMU node.
    pub rc_index: u32,
    /// PCIe segment number of the root complex this SMMU covers. Used as
    /// the output_base in the SMMU→ITS ID mapping to produce globally
    /// unique ITS device IDs: `(segment << 16) | BDF`.
    pub segment: u16,
    /// MMIO base address of the SMMU.
    pub base: u64,
    /// GIC SPI INTID for the event queue interrupt.
    pub event_gsiv: u32,
    /// GIC SPI INTID for the global error interrupt.
    pub gerr_gsiv: u32,
}

/// Binary ACPI tables constructed by [`AcpiTablesBuilder`].
pub struct BuiltAcpiTables {
    /// The RDSP. Assumed to be given a whole page.
    pub rdsp: Vec<u8>,
    /// The remaining tables pointed to by the RDSP.
    pub tables: Vec<u8>,
}

/// NUMA distance information for SLIT generation.
pub struct SlitInfo {
    /// Number of NUMA nodes (system localities).
    pub num_nodes: usize,
    /// Explicit distance entries (src, dst, distance).
    /// Entries not specified default to 10 (self) or 20 (cross-node).
    pub distances: Vec<(u32, u32, u8)>,
}

/// Builder to construct a set of [`BuiltAcpiTables`]
pub struct AcpiTablesBuilder<'a, T: AcpiTopology> {
    /// The processor topology.
    ///
    /// It is assumed that the MADT processor UID should start at 1 and enumerate each
    /// of these APIC IDs in turn.
    pub processor_topology: &'a ProcessorTopology<T>,
    /// The memory layout of the VM.
    pub mem_layout: &'a MemoryLayout,
    /// The cache topology of the VM.
    ///
    /// If and only if this is set, then the PPTT table will be generated.
    pub cache_topology: Option<&'a CacheTopology>,
    /// The PCIe topology.
    ///
    /// If and only if this has root complexes, then an MCFG will be generated.
    pub pcie_host_bridges: &'a Vec<PcieHostBridge>,
    /// NUMA distance information for SLIT generation.
    ///
    /// If set, a SLIT table will be generated.
    pub slit_info: Option<&'a SlitInfo>,
    /// Architecture-specific ACPI configuration.
    pub arch: AcpiArchConfig,
}

/// Configuration for AMD IOMMU ACPI table (IVRS) generation.
#[derive(Clone, Debug)]
pub struct AmdIommuAcpiConfig {
    /// PCI DeviceID (BDF) of the IOMMU, encoded as `(bus << 8) | (dev << 3) | fn`.
    pub device_id: u16,
    /// Offset of the AMD IOMMU capability block in PCI config space.
    pub capability_offset: u16,
    /// MMIO base address of the IOMMU register region.
    pub mmio_base: u64,
    /// PCI segment group number (typically 0).
    pub pci_segment: u16,
    /// IOMMU feature reporting for the IVHD (should match MMIO ExtFeat register).
    pub ivhd_features: u64,
    /// Lowest bus number covered by this IOMMU.
    pub start_bus: u8,
    /// Highest bus number covered by this IOMMU.
    pub end_bus: u8,
}

/// IVRS-level configuration for AMD IOMMU ACPI table generation.
///
/// Groups the IVRS header fields (PA/VA sizes) with the per-IOMMU configs.
#[derive(Clone, Debug)]
pub struct AmdIommuIvrsConfig {
    /// Physical address size in bits (e.g. 48). Written to the IVRS IVinfo header.
    pub pa_size: u8,
    /// Virtual address size in bits (e.g. 48). Written to the IVRS IVinfo header.
    pub va_size: u8,
    /// Per-IOMMU configurations, one per root complex with an AMD IOMMU.
    pub iommus: Vec<AmdIommuAcpiConfig>,
}

/// Architecture-specific ACPI configuration carried by [`AcpiTablesBuilder`].
pub enum AcpiArchConfig {
    /// x86-specific settings (IOAPIC, PIC, PIT, PSP, PM base, SCI IRQ).
    X86 {
        /// If an IOAPIC is present.
        with_ioapic: bool,
        /// If a PIC is present.
        with_pic: bool,
        /// If a PIT is present.
        with_pit: bool,
        /// If a PSP is present.
        with_psp: bool,
        /// Base address of dynamic power management device registers.
        pm_base: u16,
        /// ACPI IRQ number.
        acpi_irq: u32,
        /// AMD IOMMU IVRS table configuration. If `Some`, an IVRS table is
        /// generated with one IVHD block per IOMMU instance.
        amd_iommu: Option<AmdIommuIvrsConfig>,
    },
    /// ARM64-specific settings (HW_REDUCED_ACPI FADT).
    Aarch64 {
        /// Hypervisor vendor identity for the FADT.
        /// Zero when not running under a hypervisor.
        hypervisor_vendor_identity: u64,
        /// Virtual timer PPI (GIC INTID).
        virt_timer_ppi: u32,
        /// SMMUv3 instances. Each entry adds an SMMUv3 IORT node for the
        /// specified PCI segment. Empty means no SMMU.
        smmu: Vec<AcpiSmmuConfig>,
    },
}

pub const OEM_INFO: acpi::builder::OemInfo = acpi::builder::OemInfo {
    oem_id: *b"HVLITE",
    oem_tableid: *b"HVLITETB",
    oem_revision: 0,
    creator_id: *b"MSHV",
    creator_revision: 0,
};

/// Errors that can occur while building PCIe SSDT/CEDT payloads.
#[derive(Debug, Error)]
pub enum PcieAcpiBuildError {
    #[error("invalid CXL host-bridge CEDT entry for uid {uid}")]
    CedtHostBridge {
        uid: u32,
        #[source]
        source: acpi::cedt::CedtHostBridgeError,
    },
    #[error("failed to serialize CEDT ACPI table")]
    CedtSerialize(#[source] acpi::cedt::CedtSerializeError),
}

/// Serialized PCIe-related ACPI tables.
pub struct BuiltPcieAcpiTables {
    /// SSDT bytes containing PCI host-bridge namespace objects.
    pub ssdt: Vec<u8>,
    /// Optional CEDT bytes when at least one valid CXL host bridge is present.
    pub cedt: Option<Vec<u8>>,
}

/// Build PCIe SSDT/CEDT payloads from host-bridge topology.
pub fn build_pcie_acpi_tables(
    pcie_host_bridges: &[PcieHostBridge],
) -> Result<BuiltPcieAcpiTables, PcieAcpiBuildError> {
    let mut ssdt = Ssdt::new();
    let mut cedt = Cedt::new();
    let mut has_cedt_entries = false;

    for bridge in pcie_host_bridges {
        ssdt.add_pcie(acpi::ssdt::PcieHostBridgeEntry {
            index: bridge.index,
            segment: bridge.segment,
            start_bus: bridge.start_bus,
            end_bus: bridge.end_bus,
            ecam_range: bridge.ecam_range,
            low_mmio: bridge.low_mmio,
            high_mmio: bridge.high_mmio,
            cxl: bridge.cxl.is_some(),
            vnode: bridge.vnode,
            preserve_bars: bridge.preserve_bars,
        });

        if let Some(cxl) = &bridge.cxl {
            if let Err(source) = cedt.add_cxl_host_bridge(
                bridge.index,
                cxl.hdm_range,
                cxl.chbcr_range,
                cxl.hdm_window_restrictions.bits(),
            ) {
                return Err(PcieAcpiBuildError::CedtHostBridge {
                    uid: bridge.index,
                    source,
                });
            } else {
                has_cedt_entries = true;
            }
        }
    }

    let cedt = if has_cedt_entries {
        match cedt.to_bytes() {
            Ok(table) => Some(table),
            Err(source) => {
                return Err(PcieAcpiBuildError::CedtSerialize(source));
            }
        }
    } else {
        None
    };

    Ok(BuiltPcieAcpiTables {
        ssdt: ssdt.to_bytes(),
        cedt,
    })
}

pub trait AcpiTopology: ArchTopology + Inspect + Sized {
    fn extend_srat(topology: &ProcessorTopology<Self>, srat: &mut Vec<u8>);
    fn extend_madt(topology: &ProcessorTopology<Self>, madt: &mut Vec<u8>);
    fn needs_iort(_topology: &ProcessorTopology<Self>) -> bool {
        false
    }
    /// If the platform has an ITS, return its identifier for the IORT ITS
    /// Group node. Returns `None` when no ITS is present (root complex
    /// nodes will have no ID mappings).
    fn iort_its_id(_topology: &ProcessorTopology<Self>) -> Option<u32> {
        None
    }
}

/// The maximum ID that can be used for a legacy APIC ID in an ACPI table.
/// Anything bigger than this must use the x2apic format.
///
/// This isn't 0xff because that's the broadcast ID.
const MAX_LEGACY_APIC_ID: u32 = 0xfe;

impl AcpiTopology for X86Topology {
    fn extend_srat(topology: &ProcessorTopology<Self>, srat: &mut Vec<u8>) {
        for vp in topology.vps_arch() {
            if vp.apic_id <= MAX_LEGACY_APIC_ID {
                srat.extend_from_slice(
                    acpi_spec::srat::SratApic::new(vp.apic_id as u8, vp.base.vnode).as_bytes(),
                );
            } else {
                srat.extend_from_slice(
                    acpi_spec::srat::SratX2Apic::new(vp.apic_id, vp.base.vnode).as_bytes(),
                );
            }
        }
    }

    fn extend_madt(topology: &ProcessorTopology<Self>, madt: &mut Vec<u8>) {
        // Add LINT1 as the local NMI source
        madt.extend_from_slice(acpi_spec::madt::MadtLocalNmiSource::new().as_bytes());

        for vp in topology.vps_arch() {
            let uid = vp.base.vp_index.index() + 1;
            if vp.apic_id <= MAX_LEGACY_APIC_ID && uid <= u8::MAX.into() {
                madt.extend_from_slice(
                    acpi_spec::madt::MadtApic {
                        apic_id: vp.apic_id as u8,
                        acpi_processor_uid: uid as u8,
                        flags: acpi_spec::madt::MADT_APIC_ENABLED,
                        ..acpi_spec::madt::MadtApic::new()
                    }
                    .as_bytes(),
                );
            } else {
                madt.extend_from_slice(
                    acpi_spec::madt::MadtX2Apic {
                        x2_apic_id: vp.apic_id,
                        acpi_processor_uid: uid,
                        flags: acpi_spec::madt::MADT_APIC_ENABLED,
                        ..acpi_spec::madt::MadtX2Apic::new()
                    }
                    .as_bytes(),
                );
            }
        }
    }
}

impl AcpiTopology for Aarch64Topology {
    fn extend_srat(topology: &ProcessorTopology<Self>, srat: &mut Vec<u8>) {
        for vp in topology.vps_arch() {
            srat.extend_from_slice(
                acpi_spec::srat::SratGicc::new(vp.base.vp_index.index() + 1, vp.base.vnode)
                    .as_bytes(),
            );
        }
    }

    fn extend_madt(topology: &ProcessorTopology<Self>, madt: &mut Vec<u8>) {
        use vm_topology::processor::aarch64::GicVersion;

        let gic_acpi_version: u8 = match topology.gic_version() {
            GicVersion::V2 { .. } => 2,
            GicVersion::V3 { .. } => 3,
        };

        madt.extend_from_slice(
            acpi_spec::madt::MadtGicd::new(0, topology.gic_distributor_base(), gic_acpi_version)
                .as_bytes(),
        );
        for vp in topology.vps_arch() {
            let uid = vp.base.vp_index.index() + 1;

            // ACPI specifies that just the MPIDR affinity fields should be included.
            let mpidr = u64::from(vp.mpidr) & u64::from(aarch64defs::MpidrEl1::AFFINITY_MASK);

            let mut gicc = acpi_spec::madt::MadtGicc::new(uid, mpidr);

            if let Some(gicr) = vp.gicr {
                gicc.gicr_base_address = gicr.into();
            }

            if let GicVersion::V2 { cpu_interface_base } = topology.gic_version() {
                gicc.base_address = cpu_interface_base.into();
            }

            if let Some(pmu_gsiv) = topology.pmu_gsiv() {
                gicc.performance_monitoring_gsiv = pmu_gsiv.into();
            }
            madt.extend_from_slice(gicc.as_bytes());
        }

        // GIC v2m MSI frame for PCIe MSI support.
        if let vm_topology::processor::aarch64::GicMsiController::V2m(v2m) = topology.gic_msi() {
            madt.extend_from_slice(
                acpi_spec::madt::MadtGicMsiFrame::new(
                    0,
                    v2m.frame_base,
                    v2m.spi_base as u16,
                    v2m.spi_count as u16,
                )
                .as_bytes(),
            );
        }

        // GICv3 ITS for PCIe MSI routing via LPIs.
        if let vm_topology::processor::aarch64::GicMsiController::Its(its) = topology.gic_msi() {
            madt.extend_from_slice(acpi_spec::madt::MadtGicIts::new(0, its.its_base).as_bytes());
        }
    }

    fn needs_iort(_topology: &ProcessorTopology<Self>) -> bool {
        true
    }

    fn iort_its_id(topology: &ProcessorTopology<Self>) -> Option<u32> {
        match topology.gic_msi() {
            vm_topology::processor::aarch64::GicMsiController::Its(_) => Some(0),
            _ => None,
        }
    }
}

impl<T: AcpiTopology> AcpiTablesBuilder<'_, T> {
    fn with_srat<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&acpi::builder::Table<'_>) -> R,
    {
        let mut srat_extra: Vec<u8> = Vec::new();
        T::extend_srat(self.processor_topology, &mut srat_extra);
        for range in self.mem_layout.ram() {
            srat_extra.extend_from_slice(
                acpi_spec::srat::SratMemory::new(
                    range.range.start(),
                    range.range.len(),
                    range.vnode,
                )
                .as_bytes(),
            );
        }

        (f)(&acpi::builder::Table::new_dyn(
            acpi_spec::srat::SRAT_REVISION,
            None,
            &acpi_spec::srat::SratHeader::new(),
            &[srat_extra.as_slice()],
        ))
    }

    fn build_slit_matrix(info: &SlitInfo) -> Vec<u8> {
        let n = info.num_nodes;
        let mut matrix = vec![0u8; n * n];
        // Default: 10 for self, 20 for cross-node.
        for i in 0..n {
            for j in 0..n {
                matrix[i * n + j] = if i == j { 10 } else { 20 };
            }
        }
        // Apply explicit distances.
        for &(src, dst, distance) in &info.distances {
            matrix[src as usize * n + dst as usize] = distance;
        }
        matrix
    }

    fn with_slit<F, R>(&self, info: &SlitInfo, f: F) -> R
    where
        F: FnOnce(&acpi::builder::Table<'_>) -> R,
    {
        let matrix = Self::build_slit_matrix(info);
        let header = acpi_spec::slit::SlitHeader::new(info.num_nodes as u64);
        (f)(&acpi::builder::Table::new_dyn(
            acpi_spec::slit::SLIT_REVISION,
            None,
            &header,
            &[matrix.as_slice()],
        ))
    }

    fn with_madt<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&acpi::builder::Table<'_>) -> R,
    {
        let mut madt_extra: Vec<u8> = Vec::new();

        if let AcpiArchConfig::X86 {
            with_ioapic,
            acpi_irq,
            with_pit,
            ..
        } = self.arch
        {
            if with_ioapic {
                madt_extra.extend_from_slice(
                    acpi_spec::madt::MadtIoApic {
                        io_apic_id: 0,
                        io_apic_address: ioapic::IOAPIC_DEVICE_MMIO_REGION_BASE_ADDRESS as u32,
                        ..acpi_spec::madt::MadtIoApic::new()
                    }
                    .as_bytes(),
                );
            }

            // Add override for ACPI interrupt to be level triggered, active high.
            madt_extra.extend_from_slice(
                acpi_spec::madt::MadtInterruptSourceOverride::new(
                    acpi_irq.try_into().expect("should be in range"),
                    acpi_irq,
                    Some(InterruptPolarity::ActiveHigh),
                    Some(InterruptTriggerMode::Level),
                )
                .as_bytes(),
            );

            if with_pit {
                // IO-APIC IRQ0 is interrupt 2, which the PIT is attached to.
                madt_extra.extend_from_slice(
                    acpi_spec::madt::MadtInterruptSourceOverride::new(0, 2, None, None).as_bytes(),
                );
            }
        }

        T::extend_madt(self.processor_topology, &mut madt_extra);

        let (apic_addr, flags) = match self.arch {
            AcpiArchConfig::X86 { with_pic, .. } => (
                APIC_BASE_ADDRESS,
                if with_pic {
                    acpi_spec::madt::MADT_PCAT_COMPAT
                } else {
                    0
                },
            ),
            AcpiArchConfig::Aarch64 { .. } => (0u32, 0u32),
        };

        (f)(&acpi::builder::Table::new_dyn(
            5,
            None,
            &acpi_spec::madt::Madt { apic_addr, flags },
            &[madt_extra.as_slice()],
        ))
    }

    fn with_mcfg<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&acpi::builder::Table<'_>) -> R,
    {
        let mut mcfg_extra: Vec<u8> = Vec::new();
        for bridge in self.pcie_host_bridges {
            // Note: The topology representation of the host bridge reflects
            // the actual MMIO region regardless of starting bus number, but the
            // address reported in the MCFG table must reflect wherever bus number
            // 0 would be accessible even if the host bridge has a different starting
            // bus number.
            //
            // The layout resolver guarantees `ecam_range.start() >=
            // start_bus * 1 MiB` so this subtraction never underflows in
            // practice. Use `wrapping_sub` anyway so that, if a future code
            // path ever bypasses that check, behavior matches what a C MCFG
            // builder would do: the guest sees a wrapped base address and is
            // most likely to still compute the right per-bus ECAM addresses
            // for the buses it actually accesses.
            let ecam_region_offset = (bridge.start_bus as u64) * 256 * 4096;
            mcfg_extra.extend_from_slice(
                acpi_spec::mcfg::McfgSegmentBusRange::new(
                    bridge.ecam_range.start().wrapping_sub(ecam_region_offset),
                    bridge.segment,
                    bridge.start_bus,
                    bridge.end_bus,
                )
                .as_bytes(),
            )
        }

        (f)(&acpi::builder::Table::new_dyn(
            acpi_spec::mcfg::MCFG_REVISION,
            None,
            &acpi_spec::mcfg::McfgHeader::new(),
            &[mcfg_extra.as_slice()],
        ))
    }

    fn with_iort<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&acpi::builder::Table<'_>) -> R,
    {
        use acpi_spec::iort;

        let its_id = T::iort_its_id(self.processor_topology);
        let has_its = its_id.is_some();
        let smmu_configs: &[AcpiSmmuConfig] = match &self.arch {
            AcpiArchConfig::Aarch64 { smmu, .. } => smmu.as_slice(),
            _ => &[],
        };
        let its_node_count: u32 = if has_its { 1 } else { 0 };
        let smmu_node_count = smmu_configs.len() as u32;
        let node_count = its_node_count + smmu_node_count + self.pcie_host_bridges.len() as u32;

        let mut iort_extra: Vec<u8> = Vec::new();

        // ITS Group node comes first so other nodes can reference it.
        // The ITS Group node offset (from table start) is IORT_NODE_OFFSET.
        let its_group_offset = iort::IORT_NODE_OFFSET;
        if let Some(id) = its_id {
            iort_extra.extend_from_slice(iort::IortItsGroup::new(0, 1).as_bytes());
            // Followed by the ITS identifier (u32).
            iort_extra.extend_from_slice(&id.to_ne_bytes());
        }

        // SMMUv3 nodes come after ITS Group (if present).
        // Build a map from RC index → SMMU node offset for RC routing.
        let mut smmu_rc_offsets: Vec<(u32, u32)> = Vec::new();
        for cfg in smmu_configs {
            let smmu_node_offset = iort::IORT_NODE_OFFSET + iort_extra.len() as u32;
            smmu_rc_offsets.push((cfg.rc_index, smmu_node_offset));

            if has_its {
                // The SMMUv3 node needs two ID mappings when ITS is present:
                //
                // [0] Range mapping: translates PCI device stream IDs through
                //     the SMMU to the ITS. Used by iort_node_map_id() during
                //     RC → SMMUv3 → ITS traversal for PCI MSI domain discovery.
                //
                // [1] Single mapping: identifies the ITS group for the SMMU's
                //     own MSI domain lookup. Referenced by
                //     device_id_mapping_index. Linux's iort_set_device_domain()
                //     requires IORT_ID_SINGLE_MAPPING flag on this entry.
                //
                // Both mappings are needed even though the SMMU uses wired SPIs
                // (IDR0.MSI=0, GSIVs populated) for its own interrupts. The
                // device_id_mapping is required for Linux's IORT MSI domain
                // resolution infrastructure, which is independent of the
                // SMMU's actual interrupt delivery mechanism.
                let smmu = iort::IortSmmuV3::new_with_device_id_mapping(
                    cfg.rc_index,
                    cfg.base,
                    2,
                    cfg.event_gsiv,
                    cfg.gerr_gsiv,
                    1, // device_id_mapping_index → mapping [1]
                );
                iort_extra.extend_from_slice(smmu.as_bytes());

                // Mapping [0]: range mapping for PCI device stream IDs.
                // The output_base applies the segment offset so the ITS
                // receives globally unique device IDs: (segment << 16) | BDF.
                // Stream IDs within this SMMU are plain BDFs (0-based).
                iort_extra.extend_from_slice(
                    iort::IortIdMapping::new(
                        0,                          // input_base
                        0xFFFF,                     // id_count (16-bit BDF range)
                        (cfg.segment as u32) << 16, // output_base
                        its_group_offset,           // output_reference → ITS group
                        0,                          // flags
                    )
                    .as_bytes(),
                );

                // Mapping [1]: single mapping for the SMMU's MSI domain.
                iort_extra.extend_from_slice(
                    iort::IortIdMapping::new(
                        0,                            // input_base (unused)
                        0,                            // id_count (unused)
                        0,                            // output_base (device ID)
                        its_group_offset,             // output_reference → ITS group
                        iort::IORT_ID_SINGLE_MAPPING, // flags
                    )
                    .as_bytes(),
                );
            } else {
                let smmu =
                    iort::IortSmmuV3::new(cfg.rc_index, cfg.base, 0, cfg.event_gsiv, cfg.gerr_gsiv);
                iort_extra.extend_from_slice(smmu.as_bytes());
            }
        }

        for bridge in self.pcie_host_bridges {
            // Determine the target node for this RC's ID mapping:
            // - If this RC has an SMMU, route to the SMMU node.
            // - Otherwise, if an ITS is present, route directly to the ITS.
            // - Otherwise, no mapping (mapping_count = 0).
            let smmu_offset = smmu_rc_offsets
                .iter()
                .find(|(idx, _)| *idx == bridge.index)
                .map(|(_, off)| *off);

            let (rc_mapping_count, rc_target_offset, rc_has_smmu) = if let Some(off) = smmu_offset {
                (1, off, true)
            } else if has_its {
                (1, its_group_offset, false)
            } else {
                (0, 0, false)
            };

            let rc = iort::IortPciRootComplex::new(bridge.index, bridge.segment, rc_mapping_count);
            iort_extra.extend_from_slice(rc.as_bytes());

            if rc_mapping_count > 0 {
                // When the RC has an SMMU, output_base is 0 because stream
                // IDs are plain BDFs within the per-RC SMMU. The segment
                // offset is applied in the SMMU→ITS mapping instead.
                // When the RC goes directly to the ITS, output_base embeds
                // the segment for globally unique ITS device IDs.
                let output_base = if rc_has_smmu {
                    0
                } else {
                    (bridge.segment as u32) << 16
                };

                iort_extra.extend_from_slice(
                    iort::IortIdMapping::new(
                        0,                // input_base
                        0xFFFF,           // id_count (full 16-bit BDF range)
                        output_base,      // output_base
                        rc_target_offset, // output_reference
                        0,                // flags
                    )
                    .as_bytes(),
                );
            }
        }

        (f)(&acpi::builder::Table::new_dyn(
            iort::IORT_REVISION,
            None,
            &iort::Iort::new(node_count),
            &[iort_extra.as_slice()],
        ))
    }

    fn should_build_iort(&self) -> bool {
        T::needs_iort(self.processor_topology) && !self.pcie_host_bridges.is_empty()
    }

    fn with_ivrs<F, R>(&self, ivrs_config: &AmdIommuIvrsConfig, f: F) -> R
    where
        F: FnOnce(&acpi::builder::Table<'_>) -> R,
    {
        use acpi_spec::ivrs;

        let mut ivrs_extra: Vec<u8> = Vec::new();

        for config in &ivrs_config.iommus {
            // Use a device range entry to cover the bus range owned by this
            // root complex's IOMMU (IVHD_DEV_RANGE_START + IVHD_DEV_RANGE_END).
            // This correctly supports multiple IOMMUs within a single PCI
            // segment, each covering its own bus range.
            let dev_entries_size = 2 * size_of::<ivrs::IvhdDeviceEntry4>();
            let ivhd_total = size_of::<ivrs::IvhdType40>() + dev_entries_size;

            // Type 40h is the "mixed format" IVHD (§5.2.2.3) — same layout
            // as type 11h but supports both BDF and ACPI HID device entries.
            // We use it as the superset format; our entries are all BDF-based.
            let ivhd = ivrs::IvhdType40::new(
                config.device_id,
                config.capability_offset,
                config.mmio_base,
                config.pci_segment,
                config.ivhd_features,
            )
            .with_length(ivhd_total as u16)
            .with_flags(0); // no HT tunnel, coherent, etc.

            ivrs_extra.extend_from_slice(ivhd.as_bytes());

            let start_bdf = (config.start_bus as u16) << 8;
            let end_bdf = ((config.end_bus as u16) << 8) | 0xFF;
            ivrs_extra
                .extend_from_slice(ivrs::IvhdDeviceEntry4::range_start(start_bdf, 0).as_bytes());
            ivrs_extra.extend_from_slice(ivrs::IvhdDeviceEntry4::range_end(end_bdf).as_bytes());
        }

        let iv_info = ivrs::IvInfo::new()
            .with_efr_sup(true)
            .with_pa_size(ivrs_config.pa_size)
            .with_va_size(ivrs_config.va_size);

        (f)(&acpi::builder::Table::new_dyn(
            ivrs::IVRS_REVISION,
            None,
            &ivrs::Ivrs::new(u32::from(iv_info)),
            &[ivrs_extra.as_slice()],
        ))
    }

    fn with_pptt<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&acpi::builder::Table<'_>) -> R,
    {
        use acpi_spec::pptt;

        let cache = self.cache_topology.expect("cache topology is required");

        let current_offset =
            |pptt_extra: &[u8]| (size_of::<acpi_spec::Header>() + pptt_extra.len()) as u32;

        let cache_for = |pptt_extra: &mut Vec<u8>, level: u8, cache_type, next: Option<u32>| {
            let descriptor = cache
                .caches
                .iter()
                .find(|d| d.level == level && d.cache_type == cache_type)?;
            let offset = current_offset(pptt_extra);
            pptt_extra.extend_from_slice(
                pptt::PpttCache {
                    flags: u32::from(
                        pptt::PpttCacheFlags::new()
                            .with_size_valid(true)
                            .with_associativity_valid(true)
                            .with_cache_type_valid(true)
                            .with_line_size_valid(true),
                    )
                    .into(),
                    size: descriptor.size.into(),
                    associativity: descriptor.associativity.unwrap_or(0) as u8,
                    attributes: pptt::PpttCacheAttributes::new().with_cache_type(match descriptor
                        .cache_type
                    {
                        cache_topology::CacheType::Data => pptt::PPTT_CACHE_TYPE_DATA,
                        cache_topology::CacheType::Instruction => pptt::PPTT_CACHE_TYPE_INSTRUCTION,
                        cache_topology::CacheType::Unified => pptt::PPTT_CACHE_TYPE_UNIFIED,
                    }),
                    line_size: (descriptor.line_size as u16).into(),
                    next_level: next.unwrap_or(0).into(),
                    ..pptt::PpttCache::new()
                }
                .as_bytes(),
            );
            Some(offset)
        };

        let mut pptt_extra = Vec::new();
        let mut sockets = BTreeMap::new();
        let smt_enabled = self.processor_topology.smt_enabled();

        for vp in self.processor_topology.vps() {
            let acpi_processor_id = vp.vp_index.index() + 1;
            let info = self.processor_topology.vp_topology(vp.vp_index);

            let &mut (socket_offset, ref mut cores) =
                sockets.entry(info.socket).or_insert_with(|| {
                    let l3 =
                        cache_for(&mut pptt_extra, 3, cache_topology::CacheType::Unified, None);
                    let socket_offset = current_offset(&pptt_extra);
                    pptt_extra.extend_from_slice(
                        pptt::PpttProcessor {
                            flags: u32::from(
                                pptt::PpttProcessorFlags::new().with_physical_package(true),
                            )
                            .into(),
                            ..pptt::PpttProcessor::new(l3.is_some() as u8)
                        }
                        .as_bytes(),
                    );

                    if let Some(l3) = l3 {
                        pptt_extra.extend_from_slice(&l3.to_ne_bytes());
                    }

                    (socket_offset, BTreeMap::new())
                });

            let core_offset = *cores.entry(info.core).or_insert_with(|| {
                let l2 = cache_for(&mut pptt_extra, 2, cache_topology::CacheType::Unified, None);
                let l1i = cache_for(
                    &mut pptt_extra,
                    1,
                    cache_topology::CacheType::Instruction,
                    l2,
                );
                let l1d = cache_for(&mut pptt_extra, 1, cache_topology::CacheType::Data, l2);

                let core_offset = current_offset(&pptt_extra);
                pptt_extra.extend_from_slice(
                    pptt::PpttProcessor {
                        flags: u32::from(
                            pptt::PpttProcessorFlags::new()
                                .with_acpi_processor_uid_valid(!smt_enabled),
                        )
                        .into(),
                        acpi_processor_id: if !smt_enabled {
                            acpi_processor_id.into()
                        } else {
                            0u32.into()
                        },
                        parent: socket_offset.into(),
                        ..pptt::PpttProcessor::new(l1i.is_some() as u8 + l1d.is_some() as u8)
                    }
                    .as_bytes(),
                );

                if let Some(l1) = l1i {
                    pptt_extra.extend_from_slice(&l1.to_ne_bytes());
                }
                if let Some(l1) = l1d {
                    pptt_extra.extend_from_slice(&l1.to_ne_bytes());
                }

                core_offset
            });

            if smt_enabled {
                pptt_extra.extend_from_slice(
                    pptt::PpttProcessor {
                        flags: u32::from(
                            pptt::PpttProcessorFlags::new().with_acpi_processor_uid_valid(true),
                        )
                        .into(),
                        acpi_processor_id: acpi_processor_id.into(),
                        parent: core_offset.into(),
                        ..pptt::PpttProcessor::new(0)
                    }
                    .as_bytes(),
                )
            }
        }

        (f)(&acpi::builder::Table::new_dyn(
            1,
            None,
            &pptt::Pptt {},
            &[pptt_extra.as_slice()],
        ))
    }

    /// Build ACPI tables based on the supplied closure that adds devices to the DSDT.
    ///
    /// The RDSP is assumed to take one whole page.
    ///
    /// Returns tables that should be loaded at the supplied gpa.
    pub fn build_acpi_tables<F>(&self, gpa: u64, add_devices_to_dsdt: F) -> BuiltAcpiTables
    where
        F: FnOnce(&mut dsdt::Dsdt),
    {
        let mut dsdt_data = dsdt::Dsdt::new();
        // Name(\_S0, Package(2){0, 0})
        dsdt_data.add_object(&dsdt::NamedObject::new(
            b"\\_S0",
            &dsdt::Package(vec![0, 0]),
        ));
        // Name(\_S5, Package(2){0, 0})
        dsdt_data.add_object(&dsdt::NamedObject::new(
            b"\\_S5",
            &dsdt::Package(vec![0, 0]),
        ));
        // Add any chipset devices.
        add_devices_to_dsdt(&mut dsdt_data);
        // Add processor devices:
        // Device(P###) { Name(_HID, "ACPI0007") Name(_UID, #) Method(_STA, 0) { Return(0xF) } }
        for proc_index in 1..self.processor_topology.vp_count() + 1 {
            // To support more than 1000 processors, increment the first
            // character of the device name beyond P999.
            let c = (b'P' + (proc_index / 1000) as u8) as char;
            let name = &format!("{c}{:03}", proc_index % 1000);
            let mut proc = dsdt::Device::new(name.as_bytes());
            proc.add_object(&dsdt::NamedString::new(b"_HID", b"ACPI0007"));
            proc.add_object(&dsdt::NamedInteger::new(b"_UID", proc_index as u64));
            let mut method = dsdt::Method::new(b"_STA");
            method.add_operation(&dsdt::ReturnOp {
                result: dsdt::encode_integer(0xf),
            });
            proc.add_object(&method);
            dsdt_data.add_object(&proc);
        }

        self.build_acpi_tables_inner(gpa, &dsdt_data.to_bytes())
    }

    /// Build ACPI tables based on the supplied custom DSDT.
    ///
    /// The RDSP is assumed to take one whole page.
    ///
    /// Returns tables that should be loaded at the supplied gpa.
    pub fn build_acpi_tables_custom_dsdt(&self, gpa: u64, dsdt: &[u8]) -> BuiltAcpiTables {
        self.build_acpi_tables_inner(gpa, dsdt)
    }

    fn build_acpi_tables_inner(&self, gpa: u64, dsdt: &[u8]) -> BuiltAcpiTables {
        let mut b = acpi::builder::Builder::new(gpa + 0x1000, OEM_INFO);

        let dsdt = b.append_raw(dsdt);

        if let AcpiArchConfig::X86 {
            pm_base, acpi_irq, ..
        } = self.arch
        {
            use acpi_spec::fadt::AddressSpaceId;
            use acpi_spec::fadt::AddressWidth;
            use acpi_spec::fadt::GenericAddress;

            b.append(&acpi::builder::Table::new(
                6,
                None,
                &acpi_spec::fadt::Fadt {
                    flags: acpi_spec::fadt::FADT_WBINVD
                        | acpi_spec::fadt::FADT_PROC_C1
                        | acpi_spec::fadt::FADT_PWR_BUTTON
                        | acpi_spec::fadt::FADT_SLP_BUTTON
                        | acpi_spec::fadt::FADT_RTC_S4
                        | acpi_spec::fadt::FADT_TMR_VAL_EXT
                        | acpi_spec::fadt::FADT_RESET_REG_SUP
                        | acpi_spec::fadt::FADT_USE_PLATFORM_CLOCK,
                    x_dsdt: dsdt,
                    sci_int: acpi_irq as u16,
                    p_lvl2_lat: 101,  // disable C2
                    p_lvl3_lat: 1001, // disable C3
                    pm1_evt_len: 4,
                    x_pm1a_evt_blk: GenericAddress {
                        addr_space_id: AddressSpaceId::SystemIo,
                        register_bit_width: 32,
                        register_bit_offset: 0,
                        access_size: AddressWidth::Word,
                        address: (pm_base + chipset::pm::DynReg::STATUS.0 as u16).into(),
                    },
                    pm1_cnt_len: 2,
                    x_pm1a_cnt_blk: GenericAddress {
                        addr_space_id: AddressSpaceId::SystemIo,
                        register_bit_width: 16,
                        register_bit_offset: 0,
                        access_size: AddressWidth::Word,
                        address: (pm_base + chipset::pm::DynReg::CONTROL.0 as u16).into(),
                    },
                    gpe0_blk_len: 4,
                    x_gpe0_blk: GenericAddress {
                        addr_space_id: AddressSpaceId::SystemIo,
                        register_bit_width: 32,
                        register_bit_offset: 0,
                        access_size: AddressWidth::Word,
                        address: (pm_base + chipset::pm::DynReg::GEN_PURPOSE_STATUS.0 as u16)
                            .into(),
                    },
                    reset_reg: GenericAddress {
                        addr_space_id: AddressSpaceId::SystemIo,
                        register_bit_width: 8,
                        register_bit_offset: 0,
                        access_size: AddressWidth::Byte,
                        address: (pm_base + chipset::pm::DynReg::RESET.0 as u16).into(),
                    },
                    reset_value: chipset::pm::RESET_VALUE,
                    pm_tmr_len: 4,
                    x_pm_tmr_blk: GenericAddress {
                        addr_space_id: AddressSpaceId::SystemIo,
                        register_bit_width: 32,
                        register_bit_offset: 0,
                        access_size: AddressWidth::Dword,
                        address: (pm_base + chipset::pm::DynReg::TIMER.0 as u16).into(),
                    },
                    ..Default::default()
                },
            ));
        }

        if let AcpiArchConfig::Aarch64 {
            hypervisor_vendor_identity,
            ..
        } = self.arch
        {
            b.append(&acpi::builder::Table::new(
                6,
                None,
                &acpi_spec::fadt::Fadt {
                    flags: acpi_spec::fadt::FADT_HW_REDUCED_ACPI,
                    arm_boot_arch: 0x0003, // PSCI_COMPLIANT | PSCI_USE_HVC
                    minor_version: 3,
                    hypervisor_vendor_identity,
                    x_dsdt: dsdt,
                    ..Default::default()
                },
            ));
        }

        if let AcpiArchConfig::X86 { with_psp: true, .. } = self.arch {
            use acpi_spec::aspt;
            use acpi_spec::aspt::Aspt;
            use acpi_spec::aspt::AsptStructHeader;

            b.append(&acpi::builder::Table::new_dyn(
                1,
                None,
                &Aspt { num_structs: 3 },
                &[
                    // AspGlobalRegisters
                    AsptStructHeader::new::<aspt::structs::AspGlobalRegisters>().as_bytes(),
                    aspt::structs::AspGlobalRegisters {
                        _reserved: 0,
                        feature_register_address: psp::PSP_MMIO_ADDRESS + psp::reg::FEATURE,
                        interrupt_enable_register_address: psp::PSP_MMIO_ADDRESS + psp::reg::INT_EN,
                        interrupt_status_register_address: psp::PSP_MMIO_ADDRESS
                            + psp::reg::INT_STS,
                    }
                    .as_bytes(),
                    // SevMailboxRegisters
                    AsptStructHeader::new::<aspt::structs::SevMailboxRegisters>().as_bytes(),
                    aspt::structs::SevMailboxRegisters {
                        mailbox_interrupt_id: 1,
                        _reserved: [0; 3],
                        cmd_resp_register_address: psp::PSP_MMIO_ADDRESS + psp::reg::CMD_RESP,
                        cmd_buf_addr_lo_register_address: psp::PSP_MMIO_ADDRESS
                            + psp::reg::CMD_BUF_ADDR_LO,
                        cmd_buf_addr_hi_register_address: psp::PSP_MMIO_ADDRESS
                            + psp::reg::CMD_BUF_ADDR_HI,
                    }
                    .as_bytes(),
                    // AcpiMailboxRegisters
                    AsptStructHeader::new::<aspt::structs::AcpiMailboxRegisters>().as_bytes(),
                    aspt::structs::AcpiMailboxRegisters {
                        _reserved1: 0,
                        cmd_resp_register_address: psp::PSP_MMIO_ADDRESS + psp::reg::ACPI_CMD_RESP,
                        _reserved2: [0; 2],
                    }
                    .as_bytes(),
                ],
            ));
        }

        self.with_madt(|t| b.append(t));
        self.with_srat(|t| b.append(t));
        if let Some(info) = self.slit_info {
            self.with_slit(info, |t| b.append(t));
        }
        if !self.pcie_host_bridges.is_empty() {
            self.with_mcfg(|t| b.append(t));

            if self.should_build_iort() {
                self.with_iort(|t| b.append(t));
            }

            let pcie_tables = build_pcie_acpi_tables(self.pcie_host_bridges)
                .expect("PCIe ACPI table build should not fail");
            b.append_raw(&pcie_tables.ssdt);
            if let Some(cedt) = pcie_tables.cedt {
                b.append_raw(&cedt);
            }
        }

        if self.cache_topology.is_some() {
            self.with_pptt(|t| b.append(t));
        }

        if let AcpiArchConfig::X86 {
            amd_iommu: Some(ivrs_config),
            ..
        } = &self.arch
        {
            self.with_ivrs(ivrs_config, |t| b.append(t));
        }

        if matches!(self.arch, AcpiArchConfig::Aarch64 { .. }) {
            self.with_gtdt(|t| b.append(t));
        }

        let (rdsp, tables) = b.build();

        BuiltAcpiTables { rdsp, tables }
    }

    /// Helper method to construct an MADT without constructing the rest of
    /// the ACPI tables.
    pub fn build_madt(&self) -> Vec<u8> {
        self.with_madt(|t| t.to_vec(&OEM_INFO))
    }

    /// Helper method to construct an SRAT without constructing the rest of
    /// the ACPI tables.
    pub fn build_srat(&self) -> Vec<u8> {
        self.with_srat(|t| t.to_vec(&OEM_INFO))
    }

    /// Helper method to construct a SLIT without constructing the rest of
    /// the ACPI tables. Returns `None` if no SLIT info is configured.
    pub fn build_slit(&self) -> Option<Vec<u8>> {
        self.slit_info
            .map(|info| self.with_slit(info, |t| t.to_vec(&OEM_INFO)))
    }

    /// Helper method to construct a MCFG without constructing the rest of the
    /// ACPI tables.
    pub fn build_mcfg(&self) -> Vec<u8> {
        self.with_mcfg(|t| t.to_vec(&OEM_INFO))
    }

    /// Helper method to construct an IORT without constructing the rest of the
    /// ACPI tables. Returns `None` if IORT is not needed for this configuration.
    pub fn build_iort(&self) -> Option<Vec<u8>> {
        self.should_build_iort()
            .then(|| self.with_iort(|t| t.to_vec(&OEM_INFO)))
    }

    /// Helper method to construct an IVRS without constructing the rest of the
    /// ACPI tables. Returns `None` if AMD IOMMU is not configured.
    pub fn build_ivrs(&self) -> Option<Vec<u8>> {
        if let AcpiArchConfig::X86 {
            amd_iommu: Some(ivrs_config),
            ..
        } = &self.arch
        {
            return Some(self.with_ivrs(ivrs_config, |t| t.to_vec(&OEM_INFO)));
        }
        None
    }

    /// Helper method to construct a PPTT without constructing the rest of the
    /// ACPI tables.
    ///
    /// # Panics
    /// Panics if `self.cache_topology` is not set.
    pub fn build_pptt(&self) -> Vec<u8> {
        self.with_pptt(|t| t.to_vec(&OEM_INFO))
    }

    fn with_gtdt<R>(&self, f: impl FnOnce(&acpi::builder::Table<'_>) -> R) -> R {
        let virt_timer_ppi = if let AcpiArchConfig::Aarch64 { virt_timer_ppi, .. } = self.arch {
            virt_timer_ppi
        } else {
            0
        };
        (f)(&acpi::builder::Table::new(
            3,
            None,
            &acpi_spec::gtdt::Gtdt {
                cnt_control_base: 0xFFFF_FFFF_FFFF_FFFF,
                virtual_el1_timer_gsiv: virt_timer_ppi,
                virtual_el1_timer_flags: acpi_spec::gtdt::GTDT_TIMER_ACTIVE_LOW,
                cnt_read_base: 0xFFFF_FFFF_FFFF_FFFF,
                ..Default::default()
            },
        ))
    }

    pub fn build_gtdt(&self) -> Vec<u8> {
        self.with_gtdt(|t| t.to_vec(&OEM_INFO))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use acpi_spec::madt::MadtParser;
    use acpi_spec::mcfg::parse_mcfg;
    use memory_range::MemoryRange;
    use virt::VpIndex;
    use virt::VpInfo;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::X86VpInfo;

    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    const MMIO: [MemoryRange; 2] = [
        MemoryRange::new(GB..2 * GB),
        MemoryRange::new(3 * GB..4 * GB),
    ];

    fn new_mem() -> MemoryLayout {
        MemoryLayout::new(TB, &MMIO, &[], &[], None).unwrap()
    }

    fn new_builder<'a>(
        mem_layout: &'a MemoryLayout,
        processor_topology: &'a ProcessorTopology<X86Topology>,
        pcie_host_bridges: &'a Vec<PcieHostBridge>,
    ) -> AcpiTablesBuilder<'a, X86Topology> {
        AcpiTablesBuilder {
            processor_topology,
            mem_layout,
            cache_topology: None,
            pcie_host_bridges,
            slit_info: None,
            arch: AcpiArchConfig::X86 {
                with_ioapic: true,
                with_pic: false,
                with_pit: false,
                with_psp: false,
                pm_base: 1234,
                acpi_irq: 2,
                amd_iommu: None,
            },
        }
    }

    // TODO: might be useful to test ioapic, pic, etc
    #[test]
    fn test_basic_madt_cpu() {
        let mem = new_mem();
        let topology = TopologyBuilder::new_x86().build(16).unwrap();
        let pcie = vec![];
        let builder = new_builder(&mem, &topology, &pcie);
        let madt = builder.build_madt();

        let entries = MadtParser::new(&madt).unwrap().parse_apic_ids().unwrap();
        assert_eq!(entries, (0..16).map(Some).collect::<Vec<_>>());

        let topology = TopologyBuilder::new_x86()
            .apic_id_offset(13)
            .build(16)
            .unwrap();
        let builder = new_builder(&mem, &topology, &pcie);
        let madt = builder.build_madt();

        let entries = MadtParser::new(&madt).unwrap().parse_apic_ids().unwrap();
        assert_eq!(entries, (13..29).map(Some).collect::<Vec<_>>());

        let apic_ids = [12, 58, 4823, 36];
        let topology = TopologyBuilder::new_x86()
            .build_with_vp_info(apic_ids.iter().enumerate().map(|(uid, apic)| X86VpInfo {
                base: VpInfo {
                    vp_index: VpIndex::new(uid as u32),
                    vnode: 0,
                },
                apic_id: *apic,
            }))
            .unwrap();
        let builder = new_builder(&mem, &topology, &pcie);
        let madt = builder.build_madt();

        let entries = MadtParser::new(&madt).unwrap().parse_apic_ids().unwrap();
        assert_eq!(
            entries,
            apic_ids.iter().map(|e| Some(*e)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_basic_pcie_topology() {
        let mem = new_mem();
        let topology = TopologyBuilder::new_x86().build(16).unwrap();
        let pcie_host_bridges = vec![
            PcieHostBridge {
                index: 0,
                segment: 0,
                start_bus: 0,
                end_bus: 255,
                ecam_range: MemoryRange::new(0..256 * 256 * 4096),
                low_mmio: MemoryRange::new(0..0),
                high_mmio: MemoryRange::new(0..0),
                cxl: None,
                vnode: None,
                preserve_bars: false,
            },
            PcieHostBridge {
                index: 1,
                segment: 1,
                start_bus: 32,
                end_bus: 63,
                ecam_range: MemoryRange::new(5 * GB..5 * GB + 32 * 256 * 4096),
                low_mmio: MemoryRange::new(0..0),
                high_mmio: MemoryRange::new(0..0),
                cxl: None,
                vnode: None,
                preserve_bars: false,
            },
        ];

        let builder = new_builder(&mem, &topology, &pcie_host_bridges);
        let mcfg = builder.build_mcfg();

        let mut i = 0;
        let _ = parse_mcfg(&mcfg, |sbr| match i {
            0 => {
                assert_eq!(sbr.ecam_base, 0);
                assert_eq!(sbr.segment, 0);
                assert_eq!(sbr.start_bus, 0);
                assert_eq!(sbr.end_bus, 255);
                i += 1;
            }
            1 => {
                assert_eq!(sbr.ecam_base, 5 * GB - 32 * 256 * 4096);
                assert_eq!(sbr.segment, 1);
                assert_eq!(sbr.start_bus, 32);
                assert_eq!(sbr.end_bus, 63);
                i += 1;
            }
            _ => panic!("only expected two MCFG segment bus range entries"),
        })
        .unwrap();
    }

    fn new_aarch64_its_topology() -> ProcessorTopology<Aarch64Topology> {
        use vm_topology::processor::aarch64::Aarch64PlatformConfig;
        use vm_topology::processor::aarch64::GicItsInfo;
        use vm_topology::processor::aarch64::GicMsiController;
        use vm_topology::processor::aarch64::GicVersion;

        TopologyBuilder::new_aarch64(Aarch64PlatformConfig {
            gic_distributor_base: 0xffff0000,
            gic_version: GicVersion::V3 {
                redistributors_base: 0xefff0000,
            },
            gic_msi: GicMsiController::Its(GicItsInfo {
                its_base: 0xeffc0000,
            }),
            pmu_gsiv: None,
            virt_timer_ppi: 20,
            gic_nr_irqs: 992,
        })
        .build(2)
        .unwrap()
    }

    fn new_aarch64_builder<'a>(
        mem_layout: &'a MemoryLayout,
        processor_topology: &'a ProcessorTopology<Aarch64Topology>,
        pcie_host_bridges: &'a Vec<PcieHostBridge>,
    ) -> AcpiTablesBuilder<'a, Aarch64Topology> {
        AcpiTablesBuilder {
            processor_topology,
            mem_layout,
            cache_topology: None,
            pcie_host_bridges,
            slit_info: None,
            arch: AcpiArchConfig::Aarch64 {
                hypervisor_vendor_identity: 0,
                virt_timer_ppi: 20,
                smmu: vec![],
            },
        }
    }

    fn u32_at(data: &[u8], offset: usize) -> u32 {
        u32::from_ne_bytes(data[offset..offset + 4].try_into().unwrap())
    }

    fn checksum(data: &[u8]) -> u8 {
        data.iter().fold(0, |sum, byte| sum.wrapping_add(*byte))
    }

    fn contains_signature(data: &[u8], signature: &[u8; 4]) -> bool {
        data.windows(signature.len())
            .any(|window| window == signature)
    }

    #[test]
    fn test_aarch64_iort_with_its() {
        use acpi_spec::iort;

        let mem = new_mem();
        let topology = new_aarch64_its_topology();
        let pcie_host_bridges = vec![
            PcieHostBridge {
                index: 0,
                segment: 0,
                start_bus: 0,
                end_bus: 255,
                ecam_range: MemoryRange::new(0..256 * 256 * 4096),
                low_mmio: MemoryRange::new(0xdc000000..0xe0000000),
                high_mmio: MemoryRange::new(0x1000000000..0x1040000000),
                cxl: None,
                vnode: None,
                preserve_bars: false,
            },
            PcieHostBridge {
                index: 7,
                segment: 3,
                start_bus: 32,
                end_bus: 63,
                ecam_range: MemoryRange::new(5 * GB..5 * GB + 32 * 256 * 4096),
                low_mmio: MemoryRange::new(0xe0000000..0xe4000000),
                high_mmio: MemoryRange::new(0x1040000000..0x1080000000),
                cxl: None,
                vnode: None,
                preserve_bars: false,
            },
        ];
        let builder = new_aarch64_builder(&mem, &topology, &pcie_host_bridges);

        let data = builder.build_iort().unwrap();

        // IORT header
        assert_eq!(&data[0..4], b"IORT");
        assert_eq!(u32_at(&data, 4) as usize, data.len());
        assert_eq!(checksum(&data), 0);

        // 3 nodes: 1 ITS Group + 2 Root Complexes
        assert_eq!(u32_at(&data, 36), 3);
        assert_eq!(u32_at(&data, 40), iort::IORT_NODE_OFFSET);

        // First node: ITS Group at IORT_NODE_OFFSET
        let its_node = iort::IORT_NODE_OFFSET as usize;
        assert_eq!(data[its_node], iort::IORT_NODE_TYPE_ITS_GROUP);
        // its_count = 1
        assert_eq!(u32_at(&data, its_node + 16), 1);
        // ITS identifier = 0
        assert_eq!(u32_at(&data, its_node + 20), 0);

        // Second node: Root Complex 0 (after ITS Group: 20 + 4 = 24 bytes)
        let rc0 = its_node + 24;
        assert_eq!(data[rc0], iort::IORT_NODE_TYPE_PCI_ROOT_COMPLEX);
        assert_eq!(u32_at(&data, rc0 + 4), 0); // identifier
        assert_eq!(u32_at(&data, rc0 + 8), 1); // mapping_count
        // pci_segment_number at offset 28 from node start
        assert_eq!(u32_at(&data, rc0 + 28), 0);
        // ID mapping follows the root complex node (36 bytes in)
        let mapping0 = rc0 + 36;
        assert_eq!(u32_at(&data, mapping0), 0); // input_base
        assert_eq!(u32_at(&data, mapping0 + 4), 0xFFFF); // id_count
        assert_eq!(u32_at(&data, mapping0 + 8), 0); // output_base (seg 0 << 16)
        assert_eq!(u32_at(&data, mapping0 + 12), iort::IORT_NODE_OFFSET); // -> ITS group

        // Third node: Root Complex 7
        let rc1 = mapping0 + 20;
        assert_eq!(data[rc1], iort::IORT_NODE_TYPE_PCI_ROOT_COMPLEX);
        assert_eq!(u32_at(&data, rc1 + 4), 7); // identifier
        assert_eq!(u32_at(&data, rc1 + 28), 3); // pci_segment_number
        let mapping1 = rc1 + 36;
        assert_eq!(u32_at(&data, mapping1 + 8), 3 << 16); // output_base (seg 3 << 16)
    }

    #[test]
    fn test_iort_not_built_for_x86() {
        let mem = new_mem();
        let topology = TopologyBuilder::new_x86().build(1).unwrap();
        let pcie_host_bridges = vec![PcieHostBridge {
            index: 0,
            segment: 0,
            start_bus: 0,
            end_bus: 255,
            ecam_range: MemoryRange::new(0..256 * 256 * 4096),
            low_mmio: MemoryRange::new(0xdc000000..0xe0000000),
            high_mmio: MemoryRange::new(0x1000000000..0x1040000000),
            cxl: None,
            vnode: None,
            preserve_bars: false,
        }];
        let builder = new_builder(&mem, &topology, &pcie_host_bridges);
        assert!(builder.build_iort().is_none());

        let tables = builder.build_acpi_tables(0x100000, |_| {});
        assert!(!contains_signature(&tables.tables, b"IORT"));
    }

    #[test]
    fn test_iort_not_built_without_pcie() {
        let mem = new_mem();
        let topology = new_aarch64_its_topology();
        let empty: Vec<PcieHostBridge> = Vec::new();
        let builder = new_aarch64_builder(&mem, &topology, &empty);
        assert!(builder.build_iort().is_none());
    }

    #[test]
    fn test_aarch64_acpi_tables_include_iort() {
        let mem = new_mem();
        let topology = new_aarch64_its_topology();
        let pcie_host_bridges = vec![PcieHostBridge {
            index: 0,
            segment: 0,
            start_bus: 0,
            end_bus: 255,
            ecam_range: MemoryRange::new(0..256 * 256 * 4096),
            low_mmio: MemoryRange::new(0xdc000000..0xe0000000),
            high_mmio: MemoryRange::new(0x1000000000..0x1040000000),
            cxl: None,
            vnode: None,
            preserve_bars: false,
        }];
        let builder = new_aarch64_builder(&mem, &topology, &pcie_host_bridges);

        let tables = builder.build_acpi_tables(0x100000, |_| {});
        assert!(contains_signature(&tables.tables, b"MCFG"));
        assert!(contains_signature(&tables.tables, b"IORT"));
    }

    fn new_aarch64_builder_with_smmu<'a>(
        mem_layout: &'a MemoryLayout,
        processor_topology: &'a ProcessorTopology<Aarch64Topology>,
        pcie_host_bridges: &'a Vec<PcieHostBridge>,
        smmu_base: u64,
    ) -> AcpiTablesBuilder<'a, Aarch64Topology> {
        AcpiTablesBuilder {
            processor_topology,
            mem_layout,
            cache_topology: None,
            pcie_host_bridges,
            slit_info: None,
            arch: AcpiArchConfig::Aarch64 {
                hypervisor_vendor_identity: 0,
                virt_timer_ppi: 20,
                smmu: vec![AcpiSmmuConfig {
                    rc_index: 0,
                    segment: 0,
                    base: smmu_base,
                    event_gsiv: 35,
                    gerr_gsiv: 36,
                }],
            },
        }
    }

    fn u64_at(data: &[u8], offset: usize) -> u64 {
        u64::from_ne_bytes(data[offset..offset + 8].try_into().unwrap())
    }

    fn u16_at(data: &[u8], offset: usize) -> u16 {
        u16::from_ne_bytes(data[offset..offset + 2].try_into().unwrap())
    }

    #[test]
    fn test_acpi_tables_include_cedt_when_cxl_bridge_present() {
        let mem = new_mem();
        let topology = TopologyBuilder::new_x86().build(1).unwrap();
        let pcie_host_bridges = vec![PcieHostBridge {
            index: 0,
            segment: 0,
            start_bus: 0,
            end_bus: 255,
            ecam_range: MemoryRange::new(0..256 * 256 * 4096),
            low_mmio: MemoryRange::new(0xdc000000..0xe0000000),
            high_mmio: MemoryRange::new(0x1000000000..0x1040000000),
            cxl: Some(vm_topology::pcie::PcieHostBridgeCxlInfo {
                chbcr_range: MemoryRange::new(0x1040000000..0x1040010000),
                hdm_range: MemoryRange::new(0x1000000000..0x1040000000),
                hdm_window_restrictions: Default::default(),
            }),
            vnode: None,
            preserve_bars: false,
        }];
        let builder = new_builder(&mem, &topology, &pcie_host_bridges);

        let tables = builder.build_acpi_tables(0x100000, |_| {});
        assert!(contains_signature(&tables.tables, b"CEDT"));
    }

    #[test]
    fn test_iort_with_smmu_and_its() {
        use acpi_spec::iort;

        let mem = new_mem();
        let topology = new_aarch64_its_topology();
        let smmu_base: u64 = 0xEFFA_0000;
        let pcie_host_bridges = vec![PcieHostBridge {
            index: 0,
            segment: 0,
            start_bus: 0,
            end_bus: 255,
            ecam_range: MemoryRange::new(0..256 * 256 * 4096),
            low_mmio: MemoryRange::new(0xdc000000..0xe0000000),
            high_mmio: MemoryRange::new(0x1000000000..0x1040000000),
            cxl: None,
            vnode: None,
            preserve_bars: false,
        }];
        let builder = new_aarch64_builder_with_smmu(&mem, &topology, &pcie_host_bridges, smmu_base);

        let data = builder.build_iort().unwrap();

        // IORT header
        assert_eq!(&data[0..4], b"IORT");
        assert_eq!(u32_at(&data, 4) as usize, data.len());
        assert_eq!(checksum(&data), 0);

        // 3 nodes: ITS Group + SMMUv3 + 1 RC
        assert_eq!(u32_at(&data, 36), 3);

        // First node: ITS Group at IORT_NODE_OFFSET
        let its_node = iort::IORT_NODE_OFFSET as usize;
        assert_eq!(data[its_node], iort::IORT_NODE_TYPE_ITS_GROUP);
        let its_group_size = 24usize; // 20-byte struct + 4-byte ITS ID

        // Second node: SMMUv3
        let smmu_node = its_node + its_group_size;
        assert_eq!(data[smmu_node], iort::IORT_NODE_TYPE_SMMUV3);
        // base_address at offset 16 from node start
        assert_eq!(u64_at(&data, smmu_node + 16), smmu_base);
        // flags: COHACC | DEVICEID_VALID (has ITS mappings)
        assert_eq!(
            u32_at(&data, smmu_node + 24),
            iort::IORT_SMMUV3_FLAG_COHACC | iort::IORT_SMMUV3_FLAG_DEVICEID_VALID
        );
        // model: 0 (generic)
        assert_eq!(u32_at(&data, smmu_node + 36), 0);
        // mapping_count = 2 (range + single for MSI domain)
        assert_eq!(u32_at(&data, smmu_node + 8), 2);
        // device_id_mapping_index = 1
        assert_eq!(u32_at(&data, smmu_node + 64), 1);
        // SMMU mapping [0]: range mapping for PCI device stream IDs
        let smmu_node_len = u16_at(&data, smmu_node + 1) as usize;
        let smmu_mapping_0 = smmu_node + 68; // IortSmmuV3 is 68 bytes
        assert_eq!(u32_at(&data, smmu_mapping_0 + 12), iort::IORT_NODE_OFFSET); // → ITS group
        assert_eq!(u32_at(&data, smmu_mapping_0 + 16), 0); // flags: no SINGLE_MAPPING
        // SMMU mapping [1]: single mapping for SMMU's own MSI domain
        let smmu_mapping_1 = smmu_mapping_0 + 20; // IortIdMapping is 20 bytes
        assert_eq!(u32_at(&data, smmu_mapping_1 + 12), iort::IORT_NODE_OFFSET); // → ITS group
        assert_eq!(
            u32_at(&data, smmu_mapping_1 + 16),
            iort::IORT_ID_SINGLE_MAPPING
        ); // flags

        // Third node: Root Complex
        let rc_node = smmu_node + smmu_node_len;
        assert_eq!(data[rc_node], iort::IORT_NODE_TYPE_PCI_ROOT_COMPLEX);
        assert_eq!(u32_at(&data, rc_node + 8), 1); // mapping_count
        // RC → SMMUv3 mapping
        let rc_mapping = rc_node + 36;
        assert_eq!(u32_at(&data, rc_mapping), 0); // input_base
        assert_eq!(u32_at(&data, rc_mapping + 4), 0xFFFF); // id_count
        assert_eq!(u32_at(&data, rc_mapping + 8), 0); // output_base (0: has SMMU)
        assert_eq!(u32_at(&data, rc_mapping + 12), smmu_node as u32); // → SMMUv3
    }

    #[test]
    fn test_iort_with_smmu_multi_rc() {
        use acpi_spec::iort;

        let mem = new_mem();
        let topology = new_aarch64_its_topology();
        let smmu_base: u64 = 0xEFFA_0000;
        let pcie_host_bridges = vec![
            PcieHostBridge {
                index: 0,
                segment: 0,
                start_bus: 0,
                end_bus: 255,
                ecam_range: MemoryRange::new(0..256 * 256 * 4096),
                low_mmio: MemoryRange::new(0xdc000000..0xe0000000),
                high_mmio: MemoryRange::new(0x1000000000..0x1040000000),
                cxl: None,
                vnode: None,
                preserve_bars: false,
            },
            PcieHostBridge {
                index: 1,
                segment: 2,
                start_bus: 0,
                end_bus: 63,
                ecam_range: MemoryRange::new(5 * GB..5 * GB + 64 * 256 * 4096),
                low_mmio: MemoryRange::new(0xe0000000..0xe4000000),
                high_mmio: MemoryRange::new(0x1040000000..0x1080000000),
                cxl: None,
                vnode: None,
                preserve_bars: false,
            },
        ];
        let builder = new_aarch64_builder_with_smmu(&mem, &topology, &pcie_host_bridges, smmu_base);

        let data = builder.build_iort().unwrap();

        // 4 nodes: ITS + SMMUv3 + 2 RCs
        assert_eq!(u32_at(&data, 36), 4);
        assert_eq!(checksum(&data), 0);

        // ITS Group
        let its_node = iort::IORT_NODE_OFFSET as usize;
        let its_group_size = 24usize;

        // SMMUv3 node
        let smmu_node = its_node + its_group_size;
        assert_eq!(data[smmu_node], iort::IORT_NODE_TYPE_SMMUV3);
        let smmu_node_len = u16_at(&data, smmu_node + 1) as usize;

        // RC 0: segment 0 → SMMUv3
        let rc0 = smmu_node + smmu_node_len;
        assert_eq!(data[rc0], iort::IORT_NODE_TYPE_PCI_ROOT_COMPLEX);
        let rc0_mapping = rc0 + 36;
        assert_eq!(u32_at(&data, rc0_mapping + 8), 0); // output_base (0: has SMMU)
        assert_eq!(u32_at(&data, rc0_mapping + 12), smmu_node as u32); // → SMMUv3

        // RC 1: segment 2 → ITS directly (only segment 0 uses SMMU)
        let rc0_len = u16_at(&data, rc0 + 1) as usize;
        let rc1 = rc0 + rc0_len;
        assert_eq!(data[rc1], iort::IORT_NODE_TYPE_PCI_ROOT_COMPLEX);
        let rc1_mapping = rc1 + 36;
        assert_eq!(u32_at(&data, rc1_mapping + 8), 2 << 16); // output_base seg 2
        assert_eq!(u32_at(&data, rc1_mapping + 12), its_node as u32); // → ITS group
    }

    #[test]
    fn test_iort_without_smmu_unchanged() {
        // Verify the no-SMMU case still produces RC→ITS directly (regression).
        use acpi_spec::iort;

        let mem = new_mem();
        let topology = new_aarch64_its_topology();
        let pcie_host_bridges = vec![PcieHostBridge {
            index: 0,
            segment: 0,
            start_bus: 0,
            end_bus: 255,
            ecam_range: MemoryRange::new(0..256 * 256 * 4096),
            low_mmio: MemoryRange::new(0xdc000000..0xe0000000),
            high_mmio: MemoryRange::new(0x1000000000..0x1040000000),
            cxl: None,
            vnode: None,
            preserve_bars: false,
        }];
        let builder = new_aarch64_builder(&mem, &topology, &pcie_host_bridges);

        let data = builder.build_iort().unwrap();

        // 2 nodes: ITS Group + RC (no SMMUv3)
        assert_eq!(u32_at(&data, 36), 2);

        // RC mapping points directly to ITS group
        let its_node = iort::IORT_NODE_OFFSET as usize;
        let rc_node = its_node + 24; // ITS group = 24 bytes
        assert_eq!(data[rc_node], iort::IORT_NODE_TYPE_PCI_ROOT_COMPLEX);
        let rc_mapping = rc_node + 36;
        assert_eq!(u32_at(&data, rc_mapping + 12), iort::IORT_NODE_OFFSET); // → ITS group
    }

    #[test]
    fn test_iort_smmuv3_node_fields() {
        use acpi_spec::iort;

        let mem = new_mem();
        let topology = new_aarch64_its_topology();
        let smmu_base: u64 = 0xEFFA_0000;
        let pcie_host_bridges = vec![PcieHostBridge {
            index: 0,
            segment: 0,
            start_bus: 0,
            end_bus: 255,
            ecam_range: MemoryRange::new(0..256 * 256 * 4096),
            low_mmio: MemoryRange::new(0xdc000000..0xe0000000),
            high_mmio: MemoryRange::new(0x1000000000..0x1040000000),
            cxl: None,
            vnode: None,
            preserve_bars: false,
        }];
        let builder = new_aarch64_builder_with_smmu(&mem, &topology, &pcie_host_bridges, smmu_base);

        let data = builder.build_iort().unwrap();

        let smmu_node = iort::IORT_NODE_OFFSET as usize + 24; // after ITS group
        // Node type
        assert_eq!(data[smmu_node], iort::IORT_NODE_TYPE_SMMUV3);
        // Revision
        assert_eq!(data[smmu_node + 3], iort::IORT_SMMUV3_REVISION);
        // Base address
        assert_eq!(u64_at(&data, smmu_node + 16), smmu_base);
        // Flags: COHACC | DEVICEID_VALID
        assert_eq!(
            u32_at(&data, smmu_node + 24),
            iort::IORT_SMMUV3_FLAG_COHACC | iort::IORT_SMMUV3_FLAG_DEVICEID_VALID
        );
        // Reserved
        assert_eq!(u32_at(&data, smmu_node + 28), 0);
        // VATOS address = 0
        assert_eq!(u64_at(&data, smmu_node + 32), 0);
        // Model = 0 (generic)
        assert_eq!(
            u32_at(&data, smmu_node + 40),
            iort::IORT_SMMUV3_MODEL_GENERIC
        );
        // GSIVs: wired SPIs for event and gerror
        assert_eq!(u32_at(&data, smmu_node + 44), 35); // event_gsiv
        assert_eq!(u32_at(&data, smmu_node + 48), 0); // pri_gsiv
        assert_eq!(u32_at(&data, smmu_node + 52), 36); // gerr_gsiv
        assert_eq!(u32_at(&data, smmu_node + 56), 0); // sync_gsiv
    }

    fn set_amd_iommu(
        builder: &mut AcpiTablesBuilder<'_, X86Topology>,
        configs: Vec<AmdIommuAcpiConfig>,
    ) {
        if let AcpiArchConfig::X86 { amd_iommu, .. } = &mut builder.arch {
            *amd_iommu = Some(AmdIommuIvrsConfig {
                pa_size: 48,
                va_size: 48,
                iommus: configs,
            });
        } else {
            panic!("expected X86 arch config");
        }
    }

    #[test]
    fn test_ivrs_basic() {
        let mem = new_mem();
        let topology = TopologyBuilder::new_x86().build(4).unwrap();
        let pcie = vec![];
        let mut builder = new_builder(&mem, &topology, &pcie);
        set_amd_iommu(
            &mut builder,
            vec![AmdIommuAcpiConfig {
                device_id: 0x0000, // bus 0, dev 0, fn 0
                capability_offset: 0x40,
                mmio_base: 0xFD00_0000,
                pci_segment: 0,
                ivhd_features: 0xC0,
                start_bus: 0,
                end_bus: 255,
            }],
        );

        let ivrs = builder.build_ivrs().unwrap();

        // Verify IVRS signature in the first 4 bytes of the table
        assert_eq!(&ivrs[0..4], b"IVRS");
        // Verify checksum
        assert_eq!(checksum(&ivrs), 0);

        // After the 36-byte ACPI header and 12-byte IVRS header (offset 48),
        // the IVHD type 40h block starts.
        let ivhd_offset = 48;
        assert_eq!(ivrs[ivhd_offset], 0x40); // IVHD type 40h

        // IOMMU DeviceID at offset +4 (u16)
        let dev_id = u16::from_ne_bytes(ivrs[ivhd_offset + 4..ivhd_offset + 6].try_into().unwrap());
        assert_eq!(dev_id, 0x0000);

        // Capability offset at offset +6 (u16)
        let cap_offset =
            u16::from_ne_bytes(ivrs[ivhd_offset + 6..ivhd_offset + 8].try_into().unwrap());
        assert_eq!(cap_offset, 0x40);

        // MMIO base at offset +8 (u64)
        let mmio_base =
            u64::from_ne_bytes(ivrs[ivhd_offset + 8..ivhd_offset + 16].try_into().unwrap());
        assert_eq!(mmio_base, 0xFD00_0000);

        // EFR at offset +24 (u64) in the type 40h extended fields
        let efr = u64::from_ne_bytes(ivrs[ivhd_offset + 24..ivhd_offset + 32].try_into().unwrap());
        assert_eq!(efr, 0xC0); // IASup + GASup

        // Device entries follow the IVHD type 40h header (40 bytes).
        // We emit a range_start + range_end pair.
        let dev_entry_offset = ivhd_offset + 40;
        assert_eq!(ivrs[dev_entry_offset], 0x03); // range_start entry
        assert_eq!(ivrs[dev_entry_offset + 4], 0x04); // range_end entry
    }

    #[test]
    fn test_ivrs_not_generated_when_disabled() {
        let mem = new_mem();
        let topology = TopologyBuilder::new_x86().build(4).unwrap();
        let pcie = vec![];
        let builder = new_builder(&mem, &topology, &pcie);

        // amd_iommu is empty by default
        assert!(builder.build_ivrs().is_none());

        let tables = builder.build_acpi_tables(0x100000, |_| {});
        assert!(!contains_signature(&tables.tables, b"IVRS"));
    }

    #[test]
    fn test_ivrs_in_acpi_tables() {
        let mem = new_mem();
        let topology = TopologyBuilder::new_x86().build(4).unwrap();
        let pcie = vec![];
        let mut builder = new_builder(&mem, &topology, &pcie);
        set_amd_iommu(
            &mut builder,
            vec![AmdIommuAcpiConfig {
                device_id: 0x0000,
                capability_offset: 0x40,
                mmio_base: 0xFD00_0000,
                pci_segment: 0,
                ivhd_features: 0xC0,
                start_bus: 0,
                end_bus: 255,
            }],
        );

        let tables = builder.build_acpi_tables(0x100000, |_| {});
        assert!(contains_signature(&tables.tables, b"IVRS"));
    }

    #[test]
    fn test_ivrs_iommu_fields() {
        let mem = new_mem();
        let topology = TopologyBuilder::new_x86().build(4).unwrap();
        let pcie = vec![];
        let mut builder = new_builder(&mem, &topology, &pcie);
        set_amd_iommu(
            &mut builder,
            vec![AmdIommuAcpiConfig {
                device_id: 0x1234,
                capability_offset: 0x80,
                mmio_base: 0xFE00_0000,
                pci_segment: 1,
                ivhd_features: 0xC0,
                start_bus: 0,
                end_bus: 255,
            }],
        );

        let ivrs = builder.build_ivrs().unwrap();

        let ivhd_offset = 48;
        // DeviceID
        let dev_id = u16::from_ne_bytes(ivrs[ivhd_offset + 4..ivhd_offset + 6].try_into().unwrap());
        assert_eq!(dev_id, 0x1234);

        // Capability offset
        let cap_offset =
            u16::from_ne_bytes(ivrs[ivhd_offset + 6..ivhd_offset + 8].try_into().unwrap());
        assert_eq!(cap_offset, 0x80);

        // MMIO base
        let mmio_base =
            u64::from_ne_bytes(ivrs[ivhd_offset + 8..ivhd_offset + 16].try_into().unwrap());
        assert_eq!(mmio_base, 0xFE00_0000);

        // PCI segment at offset +16 (u16)
        let pci_seg =
            u16::from_ne_bytes(ivrs[ivhd_offset + 16..ivhd_offset + 18].try_into().unwrap());
        assert_eq!(pci_seg, 1);
    }

    #[test]
    fn test_ivrs_multiple_iommus() {
        let mem = new_mem();
        let topology = TopologyBuilder::new_x86().build(4).unwrap();
        let pcie = vec![];
        let mut builder = new_builder(&mem, &topology, &pcie);
        set_amd_iommu(
            &mut builder,
            vec![
                AmdIommuAcpiConfig {
                    device_id: 0x0000,
                    capability_offset: 0x40,
                    mmio_base: 0xFD00_0000,
                    pci_segment: 0,
                    ivhd_features: 0xC0,
                    start_bus: 0,
                    end_bus: 127,
                },
                AmdIommuAcpiConfig {
                    device_id: 0x0000,
                    capability_offset: 0x40,
                    mmio_base: 0xFD00_4000,
                    pci_segment: 1,
                    ivhd_features: 0xC0,
                    start_bus: 0,
                    end_bus: 255,
                },
            ],
        );

        let ivrs = builder.build_ivrs().unwrap();

        // Verify IVRS signature
        assert_eq!(&ivrs[0..4], b"IVRS");
        assert_eq!(checksum(&ivrs), 0);

        // First IVHD block at offset 48 (after 36-byte ACPI header + 12-byte IVRS header)
        let ivhd0_offset = 48;
        assert_eq!(ivrs[ivhd0_offset], 0x40); // IVHD type 40h

        // Read first IVHD length to find second IVHD
        let ivhd0_len =
            u16::from_ne_bytes(ivrs[ivhd0_offset + 2..ivhd0_offset + 4].try_into().unwrap());

        // First IOMMU: segment 0, MMIO 0xFD00_0000
        let mmio0 = u64::from_ne_bytes(
            ivrs[ivhd0_offset + 8..ivhd0_offset + 16]
                .try_into()
                .unwrap(),
        );
        assert_eq!(mmio0, 0xFD00_0000);
        let seg0 = u16::from_ne_bytes(
            ivrs[ivhd0_offset + 16..ivhd0_offset + 18]
                .try_into()
                .unwrap(),
        );
        assert_eq!(seg0, 0);

        // Second IVHD block follows the first
        let ivhd1_offset = ivhd0_offset + ivhd0_len as usize;
        assert_eq!(ivrs[ivhd1_offset], 0x40); // IVHD type 40h

        // Second IOMMU: segment 1, MMIO 0xFD00_4000
        let mmio1 = u64::from_ne_bytes(
            ivrs[ivhd1_offset + 8..ivhd1_offset + 16]
                .try_into()
                .unwrap(),
        );
        assert_eq!(mmio1, 0xFD00_4000);
        let seg1 = u16::from_ne_bytes(
            ivrs[ivhd1_offset + 16..ivhd1_offset + 18]
                .try_into()
                .unwrap(),
        );
        assert_eq!(seg1, 1);
    }
}
