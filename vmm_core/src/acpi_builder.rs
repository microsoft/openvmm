// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Construct ACPI tables for a concrete VM topology

// TODO: continue to remove these hardcoded deps
use acpi::dsdt;
use acpi_spec::fadt::AddressSpaceId;
use acpi_spec::fadt::AddressWidth;
use acpi_spec::fadt::GenericAddress;
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
    /// If an ioapic is present.
    pub with_ioapic: bool,
    /// If a PIC is present.
    pub with_pic: bool,
    /// If a PIT is present.
    pub with_pit: bool,
    /// If a psp is present.
    pub with_psp: bool,
    /// base address of dynamic power management device registers
    pub pm_base: u16,
    /// ACPI IRQ number
    pub acpi_irq: u32,
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
        // GIC version 3.
        madt.extend_from_slice(
            acpi_spec::madt::MadtGicd::new(0, topology.gic_distributor_base(), 3).as_bytes(),
        );
        for vp in topology.vps_arch() {
            let uid = vp.base.vp_index.index() + 1;

            // ACPI specifies that just the MPIDR affinity fields should be included.
            let mpidr = u64::from(vp.mpidr) & u64::from(aarch64defs::MpidrEl1::AFFINITY_MASK);
            let gicr = topology.gic_redistributors_base()
                + vp.base.vp_index.index() as u64 * aarch64defs::GIC_REDISTRIBUTOR_SIZE;
            let pmu_gsiv = topology.pmu_gsiv();
            madt.extend_from_slice(
                acpi_spec::madt::MadtGicc::new(uid, mpidr, gicr, pmu_gsiv).as_bytes(),
            );
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
        if self.with_ioapic {
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
                self.acpi_irq.try_into().expect("should be in range"),
                self.acpi_irq,
                Some(InterruptPolarity::ActiveHigh),
                Some(InterruptTriggerMode::Level),
            )
            .as_bytes(),
        );

        if self.with_pit {
            // IO-APIC IRQ0 is interrupt 2, which the PIT is attached to.
            madt_extra.extend_from_slice(
                acpi_spec::madt::MadtInterruptSourceOverride::new(0, 2, None, None).as_bytes(),
            );
        }

        T::extend_madt(self.processor_topology, &mut madt_extra);

        let flags = if self.with_pic {
            acpi_spec::madt::MADT_PCAT_COMPAT
        } else {
            0
        };

        (f)(&acpi::builder::Table::new_dyn(
            5,
            None,
            &acpi_spec::madt::Madt {
                apic_addr: APIC_BASE_ADDRESS,
                flags,
            },
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
                sci_int: self.acpi_irq as u16,
                p_lvl2_lat: 101,  // disable C2
                p_lvl3_lat: 1001, // disable C3
                pm1_evt_len: 4,
                x_pm1a_evt_blk: GenericAddress {
                    addr_space_id: AddressSpaceId::SystemIo,
                    register_bit_width: 32,
                    register_bit_offset: 0,
                    access_size: AddressWidth::Word,
                    address: (self.pm_base + chipset::pm::DynReg::STATUS.0 as u16).into(),
                },
                pm1_cnt_len: 2,
                x_pm1a_cnt_blk: GenericAddress {
                    addr_space_id: AddressSpaceId::SystemIo,
                    register_bit_width: 16,
                    register_bit_offset: 0,
                    access_size: AddressWidth::Word,
                    address: (self.pm_base + chipset::pm::DynReg::CONTROL.0 as u16).into(),
                },
                gpe0_blk_len: 4,
                x_gpe0_blk: GenericAddress {
                    addr_space_id: AddressSpaceId::SystemIo,
                    register_bit_width: 32,
                    register_bit_offset: 0,
                    access_size: AddressWidth::Word,
                    address: (self.pm_base + chipset::pm::DynReg::GEN_PURPOSE_STATUS.0 as u16)
                        .into(),
                },
                reset_reg: GenericAddress {
                    addr_space_id: AddressSpaceId::SystemIo,
                    register_bit_width: 8,
                    register_bit_offset: 0,
                    access_size: AddressWidth::Byte,
                    address: (self.pm_base + chipset::pm::DynReg::RESET.0 as u16).into(),
                },
                reset_value: chipset::pm::RESET_VALUE,
                pm_tmr_len: 4,
                x_pm_tmr_blk: GenericAddress {
                    addr_space_id: AddressSpaceId::SystemIo,
                    register_bit_width: 32,
                    register_bit_offset: 0,
                    access_size: AddressWidth::Dword,
                    address: (self.pm_base + chipset::pm::DynReg::TIMER.0 as u16).into(),
                },
                ..Default::default()
            },
        ));

        if self.with_psp {
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
        }
        if self.cache_topology.is_some() {
            self.with_pptt(|t| b.append(t));
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

    /// Helper method to construct a PPTT without constructing the rest of the
    /// ACPI tables.
    ///
    /// # Panics
    /// Panics if `self.cache_topology` is not set.
    pub fn build_pptt(&self) -> Vec<u8> {
        self.with_pptt(|t| t.to_vec(&OEM_INFO))
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
            with_ioapic: true,
            with_pic: false,
            with_pit: false,
            with_psp: false,
            pm_base: 1234,
            acpi_irq: 2,
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
}
