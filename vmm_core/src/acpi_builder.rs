// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Construct ACPI tables for a concrete VM topology

// TODO: continue to remove these hardcoded deps
use acpi::dsdt;
use acpi::ssdt::Ssdt;
use acpi_spec::madt::InterruptPolarity;
use acpi_spec::madt::InterruptTriggerMode;
use cache_topology::CacheTopology;
use chipset::ioapic;
use chipset::psp;
use inspect::Inspect;
use std::collections::BTreeMap;
use vm_topology::memory::MemoryLayout;
use vm_topology::pcie::PcieHostBridge;
use vm_topology::processor::ArchTopology;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::aarch64::Aarch64Topology;
use vm_topology::processor::x86::X86Topology;
use x86defs::apic::APIC_BASE_ADDRESS;
use zerocopy::IntoBytes;

/// Binary ACPI tables constructed by [`AcpiTablesBuilder`].
pub struct BuiltAcpiTables {
    /// The RDSP. Assumed to be given a whole page.
    pub rdsp: Vec<u8>,
    /// The remaining tables pointed to by the RDSP.
    pub tables: Vec<u8>,
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
    /// Architecture-specific ACPI configuration.
    pub arch: AcpiArchConfig,
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
    },
    /// ARM64-specific settings (HW_REDUCED_ACPI FADT).
    Aarch64 {
        /// Hypervisor vendor identity for the FADT.
        /// Zero when not running under a hypervisor.
        hypervisor_vendor_identity: u64,
        /// Virtual timer PPI (GIC INTID).
        virt_timer_ppi: u32,
    },
}

pub const OEM_INFO: acpi::builder::OemInfo = acpi::builder::OemInfo {
    oem_id: *b"HVLITE",
    oem_tableid: *b"HVLITETB",
    oem_revision: 0,
    creator_id: *b"MSHV",
    creator_revision: 0,
};

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
            let ecam_region_offset = (bridge.start_bus as u64) * 256 * 4096;
            mcfg_extra.extend_from_slice(
                acpi_spec::mcfg::McfgSegmentBusRange::new(
                    bridge.ecam_range.start() - ecam_region_offset,
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
        let its_node_count: u32 = if has_its { 1 } else { 0 };
        let node_count = its_node_count + self.pcie_host_bridges.len() as u32;
        let mapping_count: u32 = if has_its { 1 } else { 0 };

        let mut iort_extra: Vec<u8> = Vec::new();

        // ITS Group node comes first so root complexes can reference it.
        // The ITS Group node offset (from table start) is IORT_NODE_OFFSET.
        let its_group_offset = iort::IORT_NODE_OFFSET;
        if let Some(id) = its_id {
            iort_extra.extend_from_slice(iort::IortItsGroup::new(0, 1).as_bytes());
            // Followed by the ITS identifier (u32).
            iort_extra.extend_from_slice(&id.to_ne_bytes());
        }

        for bridge in self.pcie_host_bridges {
            let rc = iort::IortPciRootComplex::new(bridge.index, bridge.segment, mapping_count);
            iort_extra.extend_from_slice(rc.as_bytes());

            if has_its {
                // Single ID mapping: full RID range → ITS Group node.
                // output_base uses (segment << 16) so device IDs in the
                // ITS namespace are unique across PCI segments.
                iort_extra.extend_from_slice(
                    iort::IortIdMapping::new(
                        0,                             // input_base
                        0xFFFF, // id_count (full 16-bit BDF range, minus 1 per IORT spec)
                        (bridge.segment as u32) << 16, // output_base
                        its_group_offset, // output_reference
                        0,      // flags
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
        F: FnOnce(&MemoryLayout, &mut dsdt::Dsdt),
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
        add_devices_to_dsdt(self.mem_layout, &mut dsdt_data);
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
        if !self.pcie_host_bridges.is_empty() {
            self.with_mcfg(|t| b.append(t));

            if self.should_build_iort() {
                self.with_iort(|t| b.append(t));
            }

            let mut ssdt = Ssdt::new();
            for bridge in self.pcie_host_bridges {
                ssdt.add_pcie(
                    bridge.index,
                    bridge.segment,
                    bridge.start_bus,
                    bridge.end_bus,
                    bridge.ecam_range,
                    bridge.low_mmio,
                    bridge.high_mmio,
                );
            }
            b.append_raw(&ssdt.to_bytes());
        }

        if self.cache_topology.is_some() {
            self.with_pptt(|t| b.append(t));
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
            arch: AcpiArchConfig::X86 {
                with_ioapic: true,
                with_pic: false,
                with_pit: false,
                with_psp: false,
                pm_base: 1234,
                acpi_irq: 2,
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
            },
            PcieHostBridge {
                index: 1,
                segment: 1,
                start_bus: 32,
                end_bus: 63,
                ecam_range: MemoryRange::new(5 * GB..5 * GB + 32 * 256 * 4096),
                low_mmio: MemoryRange::new(0..0),
                high_mmio: MemoryRange::new(0..0),
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
            arch: AcpiArchConfig::Aarch64 {
                hypervisor_vendor_identity: 0,
                virt_timer_ppi: 20,
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
            },
            PcieHostBridge {
                index: 7,
                segment: 3,
                start_bus: 32,
                end_bus: 63,
                ecam_range: MemoryRange::new(5 * GB..5 * GB + 32 * 256 * 4096),
                low_mmio: MemoryRange::new(0xe0000000..0xe4000000),
                high_mmio: MemoryRange::new(0x1040000000..0x1080000000),
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
        }];
        let builder = new_builder(&mem, &topology, &pcie_host_bridges);
        assert!(builder.build_iort().is_none());

        let tables = builder.build_acpi_tables(0x100000, |_, _| {});
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
        }];
        let builder = new_aarch64_builder(&mem, &topology, &pcie_host_bridges);

        let tables = builder.build_acpi_tables(0x100000, |_, _| {});
        assert!(contains_signature(&tables.tables, b"MCFG"));
        assert!(contains_signature(&tables.tables, b"IORT"));
    }
}
