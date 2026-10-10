// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functionality to prepare VTL0 to run.

use self::vtl2_config::RuntimeParameters;
use crate::loader::vtl0_config::LinuxInfo;
use crate::worker::ChipsetMmioRanges;
use crate::worker::FirmwareType;
use cvm_tracing::CVM_ALLOWED;
use guest_emulation_transport::api::platform_settings::DevicePlatformSettings;
use guest_emulation_transport::api::platform_settings::General;
use guestmem::GuestMemory;
use hvdef::HV_PAGE_SIZE;
use igvm_defs::MemoryMapEntryType;
use loader::importer::Register;
use loader::uefi::IMAGE_SIZE;
use loader::uefi::config;
use loader_defs::paravisor::PageRegionDescriptor;
use memory_range::MemoryRange;
#[cfg(guest_arch = "x86_64")]
use serial_16550_resources::ComPort;
use std::ffi::CString;
use thiserror::Error;
use vm_topology::memory::MemoryLayout;
use vm_topology::memory::MemoryRangeWithNode;
use vm_topology::processor::ProcessorTopology;
use vmm_core::acpi_builder::AcpiTablesBuilder;
use vmm_core::acpi_builder::GenericInitiator;
use vmm_core::acpi_builder::SlitInfo;
use vmotherboard::options::VmChipsetCapabilities;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

pub mod vtl0_config;
pub mod vtl2_config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadKind {
    None,
    Uefi,
    Pcat,
    Linux,
}

impl From<LoadKind> for FirmwareType {
    fn from(value: LoadKind) -> Self {
        match value {
            LoadKind::None | LoadKind::Linux => FirmwareType::None,
            LoadKind::Uefi => FirmwareType::Uefi,
            LoadKind::Pcat => FirmwareType::Pcat,
        }
    }
}

#[derive(Debug, Clone)]
pub enum VpContext {
    Vbs(Vec<Register>),
    // TODO SNP: add SNP with VMSA
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("accessing guest memory failed")]
    GuestMemoryAccess(#[source] guestmem::GuestMemoryError),
    #[cfg(guest_arch = "x86_64")]
    #[error("linux loader error")]
    LinuxLoader(#[source] loader::linux::Error),
    #[cfg(guest_arch = "x86_64")]
    #[error("pcat loader error")]
    PcatLoader(#[source] loader::pcat::Error),
    #[error("pcat not supported")]
    PcatSupport,
    #[error("uefi not supported")]
    UefiSupport,
    #[error("linux not supported")]
    LinuxSupport,
    #[error("finalizing boot")]
    Finalize(#[source] vtl0_config::Error),
    #[error("invalid acpi table: too short")]
    InvalidAcpiTableLength,
    #[error("duplicate ACPI override {0:?}")]
    DuplicateAcpiTable([u8; 4]),
    #[error("present non-isolated IGVM SLIT has no original table bytes")]
    MissingHostIgvmSlit,
    #[error("invalid SLIT for generated ACPI topology")]
    Slit(#[from] SlitValidationError),
    #[cfg(guest_arch = "aarch64")]
    #[error("expected GICv3 topology")]
    ExpectedGicV3,
}

/// An error validating SLIT generation inputs.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SlitValidationError {
    /// The locality count is zero or exceeds the supported range.
    #[error("invalid SLIT locality count {0}")]
    Localities(usize),
    /// The generated table length cannot be represented as `usize`.
    #[error("SLIT length calculation overflows for {0} localities")]
    SizeOverflow(usize),
    /// The generated table exceeds the caller's or ACPI's size limit.
    #[error("SLIT length {actual} exceeds limit {limit}")]
    TooLarge {
        /// The required table length.
        actual: usize,
        /// The permitted table length.
        limit: usize,
    },
    /// An explicit distance references an absent locality.
    #[error("SLIT distance index {src}->{dst} is outside {num_nodes} localities")]
    Index {
        /// The source locality.
        src: u32,
        /// The destination locality.
        dst: u32,
        /// The configured locality count.
        num_nodes: usize,
    },
    /// An explicit distance uses a reserved value or a non-10 diagonal.
    #[error("invalid SLIT distance {src}->{dst}: {distance}")]
    Distance {
        /// The source locality.
        src: u32,
        /// The destination locality.
        dst: u32,
        /// The invalid distance.
        distance: u8,
    },
    /// A locally emitted SRAT domain is not covered by the SLIT.
    #[error("SRAT domain {domain} is outside {num_nodes} SLIT localities")]
    SratDomain {
        /// The uncovered SRAT domain.
        domain: u32,
        /// The configured locality count.
        num_nodes: usize,
    },
}

pub const PV_CONFIG_BASE_PAGE: u64 = if cfg!(guest_arch = "x86_64") {
    loader_defs::paravisor::PARAVISOR_VTL0_MEASURED_CONFIG_BASE_PAGE_X64
} else if cfg!(guest_arch = "aarch64") {
    loader_defs::paravisor::PARAVISOR_VTL0_MEASURED_CONFIG_BASE_PAGE_AARCH64
} else {
    panic!("unsupported guest architecture");
};

/// Additional loader config specified at runtime via underhill launch arguments.
pub struct Config {
    /// A string to append to the current VTL0 command line. Currently only used
    /// when booting linux directly.
    pub cmdline_append: CString,
    /// Whether UEFI should disable SHA-1 PCR usage.
    pub disable_sha1_pcr: bool,
}

/// Load VTL0 based on measured config. Returns any VP state that should be set.
pub fn load(
    gm: &GuestMemory,
    mem_layout: &MemoryLayout,
    processor_topology: &ProcessorTopology,
    vtl0_memory_map: &[(MemoryRangeWithNode, MemoryMapEntryType)],
    runtime_params: &RuntimeParameters,
    chipset_capabilities: VmChipsetCapabilities,
    load_kind: LoadKind,
    vtl0_info: vtl0_config::MeasuredVtl0Info,
    platform_config: &DevicePlatformSettings,
    config: Config,
    caps: &virt::PartitionCapabilities,
    isolated: bool,
    chipset_mmio: &ChipsetMmioRanges,
) -> Result<VpContext, Error> {
    let context = match load_kind {
        LoadKind::None => {
            tracing::info!(CVM_ALLOWED, "loading nothing into VTL0");
            VpContext::Vbs(Vec::new())
        }
        LoadKind::Uefi => {
            tracing::info!(CVM_ALLOWED, "loading UEFI into VTL0");
            // UEFI image is already loaded into guest memory, so only the
            // dynamic config needs to be written.
            let uefi_info = vtl0_info.supports_uefi.as_ref().ok_or(Error::UefiSupport)?;

            write_uefi_config(
                gm,
                mem_layout,
                processor_topology,
                vtl0_memory_map,
                runtime_params,
                chipset_capabilities,
                platform_config,
                caps,
                config.disable_sha1_pcr,
                isolated,
                chipset_mmio,
            )?;
            uefi_info.vp_context.clone()
        }
        #[cfg(not(guest_arch = "x86_64"))]
        LoadKind::Linux => {
            let _ = config.cmdline_append;
            let LinuxInfo {
                kernel_range: _kernel_range,
                kernel_entrypoint: _kernel_entrypoint,
                initrd: _initrd,
                command_line: _command_line,
            } = vtl0_info
                .supports_linux
                .as_ref()
                .ok_or(Error::LinuxSupport)?;
            todo!();
        }
        #[cfg(guest_arch = "x86_64")]
        LoadKind::Linux => {
            tracing::info!(CVM_ALLOWED, "loading Linux into VTL0");

            let LinuxInfo {
                kernel_range,
                kernel_entrypoint,
                initrd,
                command_line,
            } = vtl0_info
                .supports_linux
                .as_ref()
                .ok_or(Error::LinuxSupport)?;

            // Convert the read cstring to a vec to allow appending.
            let mut command_line = command_line.clone().unwrap_or_default().into_bytes();

            // Add a trailing space to the base string so that the appended
            // string won't corrupt the last argument.
            if !command_line.is_empty() && command_line.last() != Some(&b' ') {
                command_line.push(b' ');
            }

            // Copy from the append string.
            command_line.extend_from_slice(config.cmdline_append.to_bytes());

            let command_line = CString::new(command_line).expect("constructed from valid CStrings");

            let slit_info = runtime_params
                .slit()
                .map(|slit| SlitInfo::from(&slit.parsed));
            load_linux(LoadLinuxParams {
                gm,
                mem_layout,
                processor_topology,
                platform_config,
                chipset_capabilities,
                chipset_mmio,
                kernel_range: *kernel_range,
                kernel_entrypoint: *kernel_entrypoint,
                initrd: *initrd,
                command_line,
                slit_info: slit_info.as_ref(),
            })?
        }
        LoadKind::Pcat => {
            tracing::info!(CVM_ALLOWED, "loading pcat into VTL0");

            if !vtl0_info.supports_pcat {
                return Err(Error::PcatSupport);
            }

            #[cfg(not(guest_arch = "x86_64"))]
            panic!("Not supported");

            #[cfg(guest_arch = "x86_64")]
            load_pcat(gm, mem_layout)?
        }
    };

    vtl0_info
        .finalize_load(gm, load_kind)
        .map_err(Error::Finalize)?;

    Ok(context)
}

/// Load PCAT into VTL0.
#[cfg(guest_arch = "x86_64")]
fn load_pcat(gm: &GuestMemory, mem_layout: &MemoryLayout) -> Result<VpContext, Error> {
    let mut loader = vm_loader::Loader::new(gm.clone(), mem_layout, hvdef::Vtl::Vtl0);

    // PCAT image is already loaded into guest memory, so only register state
    // needs to get set
    loader::pcat::load(&mut loader, None, mem_layout.max_ram_below_4gb())
        .map_err(Error::PcatLoader)?;

    Ok(VpContext::Vbs(loader.initial_regs()))
}

#[cfg(guest_arch = "x86_64")]
struct LoadLinuxParams<'a> {
    gm: &'a GuestMemory,
    mem_layout: &'a MemoryLayout,
    processor_topology: &'a ProcessorTopology,
    platform_config: &'a DevicePlatformSettings,
    chipset_capabilities: VmChipsetCapabilities,
    chipset_mmio: &'a ChipsetMmioRanges,
    /// The region of memory used by the kernel.
    kernel_range: MemoryRange,
    /// The entrypoint of the kernel.
    kernel_entrypoint: u64,
    /// The (base address, size in bytes) of the initrd.
    initrd: Option<(u64, u64)>,
    /// The command line to pass to the kernel.
    command_line: CString,
    slit_info: Option<&'a SlitInfo>,
}

/// Checks the locality count, table size, distance indices, and distances before
/// allocating the generated SLIT matrix.
fn validate_slit_info(info: &SlitInfo, max_table_size: usize) -> Result<(), SlitValidationError> {
    let n = info.num_nodes;
    if n == 0 || u32::try_from(n).is_err() {
        return Err(SlitValidationError::Localities(n));
    }

    let table_size = n
        .checked_mul(n)
        .and_then(|matrix_size| {
            matrix_size.checked_add(
                size_of::<acpi_spec::Header>() + size_of::<acpi_spec::slit::SlitHeader>(),
            )
        })
        .ok_or(SlitValidationError::SizeOverflow(n))?;
    let limit = max_table_size.min(u32::MAX as usize);
    if table_size > limit {
        return Err(SlitValidationError::TooLarge {
            actual: table_size,
            limit,
        });
    }

    for &(src, dst, distance) in &info.distances {
        if src as usize >= n || dst as usize >= n {
            return Err(SlitValidationError::Index {
                src,
                dst,
                num_nodes: n,
            });
        }
        if distance < 10 || src == dst && distance != 10 {
            return Err(SlitValidationError::Distance { src, dst, distance });
        }
    }
    Ok(())
}

/// Checks that processor, memory, and generic-initiator domain IDs fit within
/// the SLIT locality count.
fn validate_slit_srat_domains(
    info: &SlitInfo,
    processor_topology: &ProcessorTopology,
    mem_layout: &MemoryLayout,
    generic_initiators: &[GenericInitiator],
) -> Result<(), SlitValidationError> {
    let domains = processor_topology
        .vps()
        .map(|vp| vp.vnode)
        .chain(mem_layout.ram().iter().map(|range| range.vnode))
        .chain(generic_initiators.iter().map(|gi| gi.vnode));
    for domain in domains {
        if domain as usize >= info.num_nodes {
            return Err(SlitValidationError::SratDomain {
                domain,
                num_nodes: info.num_nodes,
            });
        }
    }
    Ok(())
}

/// Load Linux into VTL0.
#[cfg(guest_arch = "x86_64")]
fn load_linux(params: LoadLinuxParams<'_>) -> Result<VpContext, Error> {
    let LoadLinuxParams {
        gm,
        mem_layout,
        processor_topology,
        platform_config,
        chipset_capabilities,
        chipset_mmio,
        kernel_range,
        kernel_entrypoint,
        initrd,
        command_line,
        slit_info,
    } = params;

    if let Some(info) = slit_info {
        validate_slit_info(info, vtl2_config::SLIT_MAX_SIZE)?;
        validate_slit_srat_domains(info, processor_topology, mem_layout, &[])?;
    }

    let acpi_builder = AcpiTablesBuilder {
        processor_topology,
        mem_layout,
        cache_topology: None,
        pcie_host_bridges: &vec![],
        slit_info,
        generic_initiators: &[],
        arch: vmm_core::acpi_builder::AcpiArchConfig::X86 {
            with_ioapic: true, // openhcl always runs with ioapic
            with_pic: chipset_capabilities.with_pic,
            with_pit: chipset_capabilities.with_pit,
            with_psp: platform_config.general.psp_enabled,
            pm_base: chipset_resources::pm::DEFAULT_PM_PIO_BASE,
            acpi_irq: chipset_resources::pm::DEFAULT_ACPI_IRQ,
            iommu: None,
        },
    };

    // Synthesize SMBIOS tables from the host-provided platform settings so the
    // guest kernel's DMI scan finds them. Type 0 (BIOS) has no host-provided
    // source, so default identity strings are used; Type 1 (System) is
    // populated from `DevicePlatformSettings`.
    //
    // The host forwards the same identity to the UEFI firmware, but it omits any
    // empty field and lets the firmware substitute its own default. There is no
    // firmware behind the direct-boot path, so to avoid a guest seeing a blank
    // `sys_vendor`/`product_name`, empty manufacturer and product strings fall
    // back to OpenHCL defaults (mirroring the OpenVMM direct-boot loader). The
    // remaining identity fields are passed through as-is; the SMBIOS builder
    // truncates any interior NUL and treats an empty string as "no string".
    let smbios = &platform_config.smbios;
    let manufacturer = if smbios.system_manufacturer.is_empty() {
        "OpenHCL"
    } else {
        &smbios.system_manufacturer
    };
    let product_name = if smbios.system_product_name.is_empty() {
        "OpenHCL Virtual Machine"
    } else {
        &smbios.system_product_name
    };
    let smbios_tables = loader::smbios::SmbiosTables {
        bios: loader::smbios::SmbiosBiosInfo {
            vendor: "OpenHCL",
            version: "OpenHCL Direct",
            release_date: "06/19/2026",
            major: 0,
            minor: 0,
        },
        system: loader::smbios::SmbiosSystemInfo {
            manufacturer,
            product_name,
            version: &smbios.system_version,
            serial_number: &smbios.serial_number,
            sku_number: &smbios.system_sku_number,
            family: &smbios.system_family,
            // The Type 1 UUID uses the same VM BIOS GUID as the UEFI path; its
            // raw bytes go in directly with no byte-order swap.
            uuid: platform_config.general.bios_guid.into(),
        },
    };

    let mut loader = vm_loader::Loader::new(gm.clone(), mem_layout, hvdef::Vtl::Vtl0);

    let initrd_info = if let Some((initrd_base, initrd_size)) = initrd {
        let size_pages = (initrd_size + HV_PAGE_SIZE - 1) & !(HV_PAGE_SIZE - 1);

        // Accept the initrd range to detect overlaps.
        loader
            .accept_new_range(
                initrd_base / HV_PAGE_SIZE,
                size_pages,
                "linux-initrd",
                loader::importer::BootPageAcceptance::Exclusive,
            )
            .expect("should be valid range");

        Some(loader::linux::InitrdInfo {
            gpa: initrd_base,
            size: initrd_size,
        })
    } else {
        None
    };

    tracing::trace!(?initrd_info);

    // Accept the kernel range to detect overlaps.
    loader
        .accept_new_range(
            kernel_range.start() / HV_PAGE_SIZE,
            kernel_range.len() / HV_PAGE_SIZE,
            "linux-kernel",
            loader::importer::BootPageAcceptance::Exclusive,
        )
        .expect("should be valid range");

    let load_info = loader::linux::LoadInfo {
        kernel: loader::linux::KernelInfo {
            gpa: kernel_range.start(),
            size: kernel_range.len(),
            entrypoint: kernel_entrypoint,
        },
        initrd: initrd_info,
        dtb: None,
        bzimage_setup_header: None,
    };

    // The loader owns the sub-1 MB layout; we supply only the command line, a
    // builder that produces the ACPI tables at the loader's chosen address, and
    // the SMBIOS identity forwarded by the host.
    loader::linux::load_config_x86(
        &mut loader,
        &load_info,
        &command_line,
        mem_layout,
        |gpa| {
            let acpi_tables = acpi_builder.build_acpi_tables(gpa, |dsdt| {
                dsdt.add_apic();

                // Add serial ports if enabled.
                if platform_config.general.com1_enabled {
                    dsdt.add_uart(
                        b"\\_SB.UAR1",
                        b"COM1",
                        1,
                        ComPort::Com1.io_port(),
                        ComPort::Com1.irq().into(),
                    );
                }

                if platform_config.general.com2_enabled {
                    dsdt.add_uart(
                        b"\\_SB.UAR2",
                        b"COM2",
                        2,
                        ComPort::Com2.io_port(),
                        ComPort::Com2.irq().into(),
                    );
                }

                dsdt.add_mmio_module(chipset_mmio.low, chipset_mmio.high);
                // TODO: change this once PCI is running in underhill
                dsdt.add_vmbus(false, None);
                dsdt.add_rtc();
            });
            loader::linux::AcpiTables {
                rsdp: acpi_tables.rsdp,
                tables: acpi_tables.tables,
            }
        },
        Some(smbios_tables),
        None,
    )
    .map_err(Error::LinuxLoader)?;

    Ok(VpContext::Vbs(loader.initial_regs()))
}

fn convert_range_type_flag(entry_type: MemoryMapEntryType) -> u32 {
    match entry_type {
        MemoryMapEntryType::MEMORY | MemoryMapEntryType::VTL2_PROTECTABLE => 0,
        MemoryMapEntryType::PLATFORM_RESERVED => config::VM_MEMORY_RANGE_FLAG_PLATFORM_RESERVED,
        // Note: this is needed when support for persistent memory is added.
        // IGVM_VHF_MEMORY_MAP_ENTRY_TYPE_PERSISTENT => VM_MEMORY_RANGE_FLAG_PERSISTENT,
        MemoryMapEntryType::PERSISTENT => {
            unimplemented!("underhill does not support persistent memory type")
        }
        MemoryMapEntryType::SPECIFIC_PURPOSE => config::VM_MEMORY_RANGE_FLAG_SPECIFIC_PURPOSE,
        _ => panic!("bad memory range type {:?}", entry_type),
    }
}

/// Write the UEFI config blob into guest memory.
pub fn write_uefi_config(
    gm: &GuestMemory,
    mem_layout: &MemoryLayout,
    processor_topology: &ProcessorTopology,
    vtl0_memory_map: &[(MemoryRangeWithNode, MemoryMapEntryType)],
    igvm_parameters: &RuntimeParameters,
    chipset_capabilities: VmChipsetCapabilities,
    platform_config: &DevicePlatformSettings,
    caps: &virt::PartitionCapabilities,
    disable_sha1_pcr: bool,
    isolated: bool,
    chipset_mmio: &ChipsetMmioRanges,
) -> Result<(), Error> {
    use guest_emulation_transport::api::platform_settings::UefiConsoleMode;

    // The bios config consists of information that comes from a few different sources...
    let mut cfg = config::Blob::new();

    // - Data that we generate ourselves
    cfg.add(&config::Entropy({
        let mut entropy = [0; 64];
        getrandom::fill(&mut entropy).expect("rng failure");
        entropy
    }));

    let mut build_madt = true;
    let mut build_srat = true;
    let mut get_provided_slit = false;
    let mut get_provided_pptt = false;

    #[cfg(not(guest_arch = "x86_64"))]
    let _ = chipset_capabilities;

    // ACPI tables that come from the DevicePlatformSettings
    // We can only trust these tables from the host if this is not an isolated VM
    if !isolated {
        for table in &platform_config.acpi_tables {
            let (header, _) = acpi_spec::Header::read_from_prefix(table)
                .map_err(|_| Error::InvalidAcpiTableLength)?;
            match &header.signature {
                b"APIC" => {
                    if !build_madt {
                        return Err(Error::DuplicateAcpiTable(header.signature));
                    }
                    build_madt = false;
                }
                b"SRAT" => {
                    if !build_srat {
                        return Err(Error::DuplicateAcpiTable(header.signature));
                    }
                    build_srat = false;
                }
                b"SLIT" => {
                    if get_provided_slit {
                        return Err(Error::DuplicateAcpiTable(header.signature));
                    }
                    get_provided_slit = true;
                }
                b"PPTT" => {
                    if get_provided_pptt {
                        return Err(Error::DuplicateAcpiTable(header.signature));
                    }
                    get_provided_pptt = true;
                }
                _ => {}
            }
            cfg.add_raw(config::BlobStructureType::AcpiTable, table);
        }
    }

    // - Data that comes from the IGVM parameters

    if build_madt || build_srat {
        let slit_info = igvm_parameters
            .slit()
            .map(|slit| SlitInfo::from(&slit.parsed));

        let acpi_builder = AcpiTablesBuilder {
            processor_topology,
            mem_layout,
            cache_topology: None,
            pcie_host_bridges: &vec![],
            slit_info: slit_info.as_ref(),
            generic_initiators: &[],
            #[cfg(guest_arch = "x86_64")]
            arch: vmm_core::acpi_builder::AcpiArchConfig::X86 {
                with_ioapic: true,
                with_pic: chipset_capabilities.with_pic,
                with_pit: chipset_capabilities.with_pit,
                with_psp: platform_config.general.psp_enabled,
                pm_base: chipset_resources::pm::DEFAULT_PM_PIO_BASE,
                acpi_irq: chipset_resources::pm::DEFAULT_ACPI_IRQ,
                iommu: None,
            },
            #[cfg(guest_arch = "aarch64")]
            arch: vmm_core::acpi_builder::AcpiArchConfig::Aarch64 {
                // Not used for MADT/SRAT generation; only matters for FADT.
                hypervisor_vendor_identity: 0,
                virt_timer_ppi: processor_topology.virt_timer_ppi(),
                smmu: Vec::new(),
            },
        };

        // Build the ACPI tables as specified.
        if build_madt {
            cfg.add_raw(
                config::BlobStructureType::AcpiTable,
                &acpi_builder.build_madt(),
            );
        }

        if build_srat {
            cfg.add_raw(
                config::BlobStructureType::AcpiTable,
                &acpi_builder.build_srat(),
            );
        }
        if !get_provided_slit {
            if let Some(info) = &slit_info {
                validate_slit_info(info, vtl2_config::SLIT_MAX_SIZE)?;
                if build_srat {
                    validate_slit_srat_domains(info, processor_topology, mem_layout, &[])?;
                }
            }
            if let Some(slit) = acpi_builder.build_slit() {
                cfg.add_raw(config::BlobStructureType::AcpiTable, &slit);
            }
        }
    } else if !isolated && !get_provided_slit {
        // The host supplied both MADT and SRAT. Keep the original IGVM SLIT
        // until the host also supplies a SLIT override.
        if let Some(slit) = igvm_parameters.slit() {
            let bytes = slit
                .host_igvm_parameter
                .as_deref()
                .ok_or(Error::MissingHostIgvmSlit)?;
            cfg.add_raw(config::BlobStructureType::AcpiTable, bytes);
        }
    }

    {
        cfg.add_raw(
            config::BlobStructureType::MemoryMap,
            vtl0_memory_map
                .iter()
                .map(|(range, typ)| config::MemoryRangeV5 {
                    base_address: range.range.start(),
                    length: range.range.len(),
                    flags: convert_range_type_flag(*typ),
                    reserved: 0,
                })
                .collect::<Vec<_>>()
                .as_bytes(),
        )
        .add_raw(
            config::BlobStructureType::MmioRanges,
            [chipset_mmio.low, chipset_mmio.high]
                .iter()
                .map(|range| config::Mmio {
                    mmio_page_number_start: range.start() / HV_PAGE_SIZE,
                    mmio_size_in_pages: range.len() / HV_PAGE_SIZE,
                })
                .collect::<Vec<_>>()
                .as_bytes(),
        )
        .add(&config::ProcessorInformation {
            max_processor_count: processor_topology.vp_count(),
            processor_count: processor_topology.vp_count(),
            processors_per_virtual_socket: processor_topology.reserved_vps_per_socket(),
            threads_per_processor: if processor_topology.smt_enabled() {
                2
            } else {
                1
            },
        });

        // TODO: Validate and reconstruct PPTT before enabling it for CCA guests.
        if !isolated && !get_provided_pptt {
            if let Some(pptt) = igvm_parameters.pptt() {
                cfg.add_raw(config::BlobStructureType::AcpiTable, pptt);
            }
        }
    }

    cfg.add(&config::BiosInformation {
        bios_size_pages: (IMAGE_SIZE / HV_PAGE_SIZE) as u32,
        flags: platform_config.general.legacy_memory_map as u32,
    })
    .add(&config::BiosGuid(platform_config.general.bios_guid))
    .add_cstring(
        config::BlobStructureType::SmbiosSystemSerialNumber,
        platform_config.smbios.serial_number.as_bytes(),
    )
    .add_cstring(
        config::BlobStructureType::SmbiosBaseSerialNumber,
        platform_config.smbios.base_board_serial_number.as_bytes(),
    )
    .add_cstring(
        config::BlobStructureType::SmbiosChassisSerialNumber,
        platform_config.smbios.chassis_serial_number.as_bytes(),
    )
    .add_cstring(
        config::BlobStructureType::SmbiosChassisAssetTag,
        platform_config.smbios.chassis_asset_tag.as_bytes(),
    );

    cfg.add(&config::NvdimmCount {
        count: platform_config.general.nvdimm_count,
        padding: [0; 3],
    });

    if let Some(instance_guid) = platform_config.general.vpci_instance_filter {
        cfg.add(&config::VpciInstanceFilter { instance_guid });
    }

    cfg.add_cstring(
        config::BlobStructureType::SmbiosSystemManufacturer,
        platform_config.smbios.system_manufacturer.as_bytes(),
    )
    .add_cstring(
        config::BlobStructureType::SmbiosSystemProductName,
        platform_config.smbios.system_product_name.as_bytes(),
    )
    .add_cstring(
        config::BlobStructureType::SmbiosSystemVersion,
        platform_config.smbios.system_version.as_bytes(),
    )
    .add_cstring(
        config::BlobStructureType::SmbiosSystemSkuNumber,
        platform_config.smbios.system_sku_number.as_bytes(),
    )
    .add_cstring(
        config::BlobStructureType::SmbiosSystemFamily,
        platform_config.smbios.system_family.as_bytes(),
    )
    .add_cstring(
        config::BlobStructureType::SmbiosBiosLockString,
        platform_config.smbios.bios_lock_string.as_bytes(),
    )
    .add_cstring(
        config::BlobStructureType::SmbiosMemoryDeviceSerialNumber,
        platform_config
            .smbios
            .memory_device_serial_number
            .as_bytes(),
    )
    .add_cstring(
        config::BlobStructureType::SmbiosProcessorManufacturer,
        &platform_config.smbios.processor_manufacturer,
    )
    .add_cstring(
        config::BlobStructureType::SmbiosProcessorVersion,
        &platform_config.smbios.processor_version,
    )
    .add(&config::Smbios31ProcessorInformation {
        processor_id: platform_config.smbios.processor_id,
        external_clock: platform_config.smbios.external_clock,
        max_speed: platform_config.smbios.max_speed,
        current_speed: platform_config.smbios.current_speed,
        processor_characteristics: platform_config.smbios.processor_characteristics,
        processor_family2: platform_config.smbios.processor_family2,
        processor_type: platform_config.smbios.processor_type,
        voltage: platform_config.smbios.voltage,
        status: platform_config.smbios.status,
        processor_upgrade: platform_config.smbios.processor_upgrade,
        reserved: 0,
    });

    // Flags is a special bit of config, as it uses information scattered across
    // many settings
    cfg.add(&{
        let mut flags = config::Flags::new();

        #[cfg(guest_arch = "x86_64")]
        flags.set_sgx_memory_enabled(caps.sgx);
        #[cfg(not(guest_arch = "x86_64"))]
        let _ = caps;

        // Frontpage is disabled if either the host requests it, or the openhcl
        // cmdline specifies it.
        flags.set_disable_frontpage(platform_config.general.disable_frontpage);

        flags.set_console(match platform_config.general.console_mode {
            UefiConsoleMode::Default => config::ConsolePort::Default,
            UefiConsoleMode::COM1 => config::ConsolePort::Com1,
            UefiConsoleMode::COM2 => config::ConsolePort::Com2,
            UefiConsoleMode::None => config::ConsolePort::None,
        });
        flags.set_tpm_enabled(platform_config.general.tpm_enabled);
        flags.set_virtual_battery_enabled(platform_config.general.battery_enabled);
        flags.set_proc_idle_enabled(platform_config.general.processor_idle_enabled);
        flags.set_serial_controllers_enabled(
            platform_config.general.com1_enabled || platform_config.general.com2_enabled,
        );
        flags.set_hibernate_enabled(platform_config.general.hibernation_enabled);
        flags.set_debugger_enabled(platform_config.general.firmware_debugging_enabled);

        flags.set_pause_after_boot_failure(platform_config.general.pause_after_boot_failure);
        flags.set_pxe_ip_v6(platform_config.general.pxe_ip_v6);
        flags.set_media_present_enabled_by_default(
            platform_config.general.media_present_enabled_by_default,
        );
        flags.set_vpci_boot_enabled(platform_config.general.vpci_boot_enabled);
        flags.set_watchdog_enabled(platform_config.general.watchdog_enabled);

        flags.set_memory_protection(determine_memory_protection_mode(
            &platform_config.general,
            isolated,
        ));

        if isolated {
            // This flag is only used inside isolated guests
            flags.set_enable_imc_when_isolated(platform_config.general.imc_enabled);
        }

        flags.set_cxl_memory_enabled(platform_config.general.cxl_memory_enabled);
        flags.set_default_boot_always_attempt(platform_config.general.default_boot_always_attempt);
        flags.set_force_dma_bounce_enabled(platform_config.general.force_dma_bounce_enabled);
        flags.set_ipmi_enabled(platform_config.general.ipmi_enabled);
        flags.set_disable_sha1_pcr(disable_sha1_pcr);

        // Some settings do not depend on host config

        // All OpenHCL vTPMs must opt-in to these settings
        flags.set_measure_additional_pcrs(true);
        flags.set_tpm_locality_regs_enabled(true);
        // OpenHCL pre-sets the MTRRs; tell the firmware
        flags.set_mtrrs_initialized_at_load(true);

        flags
    });

    #[cfg(guest_arch = "aarch64")]
    {
        use vm_topology::processor::arch::GicVersion;

        let GicVersion::V3 {
            redistributors_base,
        } = processor_topology.gic_version()
        else {
            return Err(Error::ExpectedGicV3);
        };

        cfg.add(&config::Gic {
            gic_distributor_base: processor_topology.gic_distributor_base(),
            gic_redistributors_base: redistributors_base,
        });
    }

    // Finally, with the bios config constructed, we can inject it into guest memory
    gm.write_at(loader::uefi::CONFIG_BLOB_GPA_BASE, &cfg.complete())
        .map_err(Error::GuestMemoryAccess)
}

/// Converts a [`PageRegionDescriptor`] to a [`MemoryRange`] if non-empty
fn memory_range_from_page_region(region: &PageRegionDescriptor) -> Option<MemoryRange> {
    region.pages().map(|(base_page, page_count)| {
        MemoryRange::from_4k_gpn_range(base_page..(base_page + page_count))
    })
}

fn determine_memory_protection_mode(general: &General, isolated: bool) -> config::MemoryProtection {
    use guest_emulation_transport::api::platform_settings::MemoryProtectionMode;
    use guest_emulation_transport::api::platform_settings::SecureBootTemplateType;

    let is_windows_secure_boot = general.secure_boot_enabled
        && matches!(
            general.secure_boot_template,
            SecureBootTemplateType::MicrosoftWindows
        );

    let mut requested_mode = general.memory_protection_mode;

    // CVM NOTE: While secure boot enabled is attested to, the memory protection mode is not.
    // Since we can't trust it, ensure it's always at least Default.
    if isolated
        && matches!(
            requested_mode,
            MemoryProtectionMode::Disabled | MemoryProtectionMode::Relaxed
        )
    {
        requested_mode = MemoryProtectionMode::Default;
    }

    // TODO: For now, we use secure boot template type to override what kind of memory protection mode to enable.
    //       This allows linux VMs to boot correctly as strict memory protection triggers with older versions of
    //       grub. We should revisit this in the future.
    if is_windows_secure_boot {
        match requested_mode {
            MemoryProtectionMode::Disabled => config::MemoryProtection::Disabled,
            MemoryProtectionMode::Default => config::MemoryProtection::Default,
            MemoryProtectionMode::Strict => config::MemoryProtection::Strict,
            MemoryProtectionMode::Relaxed => config::MemoryProtection::Relaxed,
        }
    } else {
        match requested_mode {
            MemoryProtectionMode::Disabled => config::MemoryProtection::Disabled,
            MemoryProtectionMode::Default
            | MemoryProtectionMode::Strict
            | MemoryProtectionMode::Relaxed => {
                // TODO: For now, Linux only ever boots with relaxed.
                config::MemoryProtection::Relaxed
            }
        }
    }
}

#[cfg(all(test, guest_arch = "x86_64"))]
mod tests {
    use super::vtl2_config::tests::{checksum, runtime, slit_bytes, table};
    use super::*;
    use guest_emulation_transport::api::platform_settings::{
        MemoryProtectionMode, PcatBootDevice, SecureBootTemplateType, Smbios, UefiConsoleMode,
    };
    use test_with_tracing::test;
    use vm_topology::processor::TopologyBuilder;

    /// Supplies host ACPI overrides with other platform settings disabled.
    fn platform(acpi_tables: Vec<Vec<u8>>) -> DevicePlatformSettings {
        DevicePlatformSettings {
            acpi_tables,
            smbios: Smbios {
                serial_number: String::new(),
                base_board_serial_number: String::new(),
                chassis_serial_number: String::new(),
                chassis_asset_tag: String::new(),
                system_manufacturer: String::new(),
                system_product_name: String::new(),
                system_version: String::new(),
                system_sku_number: String::new(),
                system_family: String::new(),
                bios_lock_string: String::new(),
                memory_device_serial_number: String::new(),
                processor_manufacturer: vec![],
                processor_version: vec![],
                processor_id: 0,
                external_clock: 0,
                max_speed: 0,
                current_speed: 0,
                processor_characteristics: 0,
                processor_family2: 0,
                processor_type: 0,
                voltage: 0,
                status: 0,
                processor_upgrade: 0,
            },
            general: General {
                secure_boot_enabled: false,
                secure_boot_template: SecureBootTemplateType::None,
                bios_guid: Default::default(),
                console_mode: UefiConsoleMode::None,
                battery_enabled: false,
                processor_idle_enabled: false,
                tpm_enabled: false,
                ipmi_enabled: false,
                com1_enabled: false,
                com1_debugger_mode: false,
                com1_vmbus_redirector: false,
                com2_enabled: false,
                com2_debugger_mode: false,
                com2_vmbus_redirector: false,
                firmware_debugging_enabled: false,
                hibernation_enabled: false,
                suppress_attestation: None,
                generation_id: None,
                legacy_memory_map: false,
                pause_after_boot_failure: false,
                pxe_ip_v6: false,
                measure_additional_pcrs: false,
                disable_frontpage: false,
                disable_sha384_pcr: false,
                media_present_enabled_by_default: false,
                vpci_boot_enabled: false,
                memory_protection_mode: MemoryProtectionMode::Default,
                default_boot_always_attempt: false,
                num_lock_enabled: false,
                pcat_boot_device_order: [PcatBootDevice::HardDrive; 4],
                vpci_instance_filter: None,
                nvdimm_count: 0,
                psp_enabled: false,
                vmbus_redirection_enabled: false,
                always_relay_host_mmio: false,
                vtl2_settings: None,
                is_servicing_scenario: false,
                watchdog_enabled: false,
                firmware_mode_is_pcat: false,
                imc_enabled: false,
                cxl_memory_enabled: false,
                efi_diagnostics_log_level: Default::default(),
                guest_state_lifetime: Default::default(),
                guest_state_encryption_policy: Default::default(),
                management_vtl_features: Default::default(),
                force_dma_bounce_enabled: false,
                hardware_sealing_policy: Default::default(),
            },
        }
    }

    /// Marks host override bytes so tests can distinguish their source.
    fn get_table(signature: &[u8; 4]) -> Vec<u8> {
        let mut bytes = if signature == b"SLIT" {
            slit_bytes()
        } else {
            table(signature, &[])
        };
        bytes[10..16].copy_from_slice(b"GETOEM");
        bytes[16..24].copy_from_slice(b"GETTABLE");
        checksum(&mut bytes);
        bytes
    }

    /// Calls the UEFI config writer and extracts ACPI tables from its guest RAM
    /// output. Checks the blob's structure count without booting a VM.
    fn emit(
        get: Vec<Vec<u8>>,
        params: &RuntimeParameters,
        isolated: bool,
        memory_domain: u32,
    ) -> anyhow::Result<Vec<Vec<u8>>> {
        let gm = GuestMemory::allocate(16 * 1024 * 1024);
        let topology = TopologyBuilder::new_x86().build(1)?;
        let memory = MemoryLayout::new_from_ranges(
            &[MemoryRangeWithNode {
                range: MemoryRange::new(0..16 * 1024 * 1024),
                vnode: memory_domain,
            }],
            &[],
        )?;
        let caps = virt::PartitionCapabilities::from_cpuid(&topology, &mut |leaf, _| match leaf {
            0 => [1, 0, 0, 0],
            1 => [0, 0, 1 << 21, 0],
            _ => [0; 4],
        })?;
        write_uefi_config(
            &gm,
            &memory,
            &topology,
            &[],
            params,
            VmChipsetCapabilities {
                with_ioapic: true,
                with_pic: false,
                with_pit: false,
                with_generic_isa_dma: false,
                with_psp: false,
                with_guest_watchdog: false,
                with_i440bx_host_pci_bridge: false,
            },
            &platform(get),
            &caps,
            false,
            isolated,
            &ChipsetMmioRanges {
                low: MemoryRange::EMPTY,
                high: MemoryRange::EMPTY,
            },
        )?;

        let base = loader::uefi::CONFIG_BLOB_GPA_BASE;
        let count: config::StructureCount =
            gm.read_plain(base + size_of::<config::Header>() as u64)?;
        let mut blob = vec![0; count.total_config_blob_size as usize];
        gm.read_at(base, &mut blob)?;
        let mut tables = vec![];
        let mut remaining = blob.as_slice();
        let mut structures = 0;
        while !remaining.is_empty() {
            let (header, body) = config::Header::read_from_prefix(remaining).unwrap();
            let length = header.length as usize;
            if header.structure_type == config::BlobStructureType::AcpiTable as u32 {
                let (acpi_header, _) = acpi_spec::Header::read_from_prefix(body).unwrap();
                let table_length = acpi_header.length.get() as usize;
                assert!(table_length <= length - size_of::<config::Header>());
                tables.push(body[..table_length].to_vec());
            }
            remaining = &remaining[length..];
            structures += 1;
        }
        assert_eq!(structures, count.total_structure_count);
        Ok(tables)
    }

    /// Identifies emitted tables in their config-blob order.
    fn signatures(tables: &[Vec<u8>]) -> Vec<[u8; 4]> {
        tables
            .iter()
            .map(|table| table[..4].try_into().unwrap())
            .collect()
    }

    /// Keeps host override order and passes through original IGVM table bytes.
    #[test]
    fn emitted_raw_slit_preserves_host_bytes_and_get_order() {
        let bytes = slit_bytes();
        let pptt = table(b"PPTT", &[]);
        let params = runtime(&bytes, &pptt, false).unwrap();
        let get = vec![
            get_table(b"APIC"),
            get_table(b"SRAT"),
            get_table(b"FACP"),
            get_table(b"SSDT"),
            get_table(b"SSDT"),
        ];
        let output = emit(get.clone(), &params, false, 0).unwrap();
        assert_eq!(&output[..get.len()], get);
        assert_eq!(
            signatures(&output),
            [
                *b"APIC", *b"SRAT", *b"FACP", *b"SSDT", *b"SSDT", *b"SLIT", *b"PPTT"
            ]
        );
        assert_eq!(output[get.len()], bytes);
        assert_eq!(output[get.len() + 1], pptt);
        assert_eq!(&output[get.len()][10..24], b"HOST  HOSTIGVM");
    }

    /// Reports missing original bytes instead of silently omitting a raw SLIT.
    #[test]
    fn raw_slit_requires_original_table_bytes() {
        let params = runtime(&slit_bytes(), &[], true).unwrap();
        let error = emit(
            vec![get_table(b"APIC"), get_table(b"SRAT")],
            &params,
            false,
            0,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<Error>(),
            Some(Error::MissingHostIgvmSlit)
        ));
    }

    /// Prefers host overrides and rejects duplicate singleton ACPI tables.
    #[test]
    fn emitted_get_precedence_and_duplicate_errors() {
        let params = runtime(&slit_bytes(), &table(b"PPTT", &[]), false).unwrap();
        for signatures_in in [
            vec![*b"SLIT", *b"PPTT"],
            vec![*b"APIC", *b"SRAT", *b"SLIT", *b"PPTT"],
            vec![*b"APIC", *b"SRAT", *b"SLIT"],
            vec![*b"APIC", *b"SRAT", *b"PPTT"],
            vec![*b"APIC", *b"SLIT"],
            vec![*b"SRAT", *b"SLIT"],
            vec![*b"PPTT"],
        ] {
            let get: Vec<_> = signatures_in.iter().map(get_table).collect();
            let output = emit(get.clone(), &params, false, 0).unwrap();
            assert_eq!(&output[..get.len()], get);
            let emitted = signatures(&output);
            for sig in [*b"SLIT", *b"PPTT", *b"APIC", *b"SRAT"] {
                assert_eq!(emitted.iter().filter(|&&s| s == sig).count(), 1);
            }
            for sig in [*b"SLIT", *b"PPTT"] {
                if let Some(index) = signatures_in.iter().position(|&s| s == sig) {
                    assert_eq!(output[index], get[index]);
                }
            }
        }
        for sig in [b"APIC", b"SRAT", b"SLIT", b"PPTT"] {
            let error = emit(vec![get_table(sig), get_table(sig)], &params, false, 0).unwrap_err();
            assert!(
                matches!(error.downcast_ref::<Error>(), Some(Error::DuplicateAcpiTable(s)) if s == sig)
            );
        }
        let error = emit(vec![vec![0]], &params, false, 0).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<Error>(),
            Some(Error::InvalidAcpiTableLength)
        ));
    }

    /// A GET SLIT takes precedence even if the unused IGVM SLIT does not cover
    /// a local domain. Isolated VMs still validate their IGVM topology.
    #[test]
    fn get_slit_bypasses_unused_igvm_domain_validation() {
        let params = runtime(&slit_bytes(), &[0; 36], false).unwrap();
        let mut body = 3u64.to_le_bytes().to_vec();
        body.extend_from_slice(&[10, 20, 20, 20, 10, 20, 20, 20, 10]);
        let get_slit = table(b"SLIT", &body);
        assert_eq!(
            acpi::slit::Slit::parse(&get_slit, vtl2_config::SLIT_MAX_SIZE)
                .unwrap()
                .num_nodes(),
            3
        );

        let output = emit(vec![get_slit.clone()], &params, false, 2).unwrap();
        assert_eq!(signatures(&output), [*b"SLIT", *b"APIC", *b"SRAT"]);
        assert_eq!(output[0], get_slit);

        for (get, isolated) in [(vec![], false), (vec![get_slit], true)] {
            let error = emit(get, &params, isolated, 2).unwrap_err();
            assert!(matches!(
                error.downcast_ref::<Error>(),
                Some(Error::Slit(SlitValidationError::SratDomain {
                    domain: 2,
                    num_nodes: 2,
                }))
            ));
        }
    }

    /// Rebuilds SLIT when either topology table is local, checking domains only
    /// when SRAT is local.
    #[test]
    fn partial_overrides_regenerate_slit_with_conditional_srat_coverage() {
        let bytes = slit_bytes();
        let params = runtime(&bytes, &[0; 36], false).unwrap();
        for (get_signature, expected) in [
            (b"APIC", [*b"APIC", *b"SRAT", *b"SLIT"]),
            (b"SRAT", [*b"SRAT", *b"APIC", *b"SLIT"]),
        ] {
            let output = emit(vec![get_table(get_signature)], &params, false, 0).unwrap();
            assert_eq!(signatures(&output), expected);
            let generated = &output[2];
            assert_ne!(generated, &bytes);
            assert_eq!(&generated[10..24], b"HVLITEHVLITETB");
            let parsed = acpi::slit::Slit::parse(generated, vtl2_config::SLIT_MAX_SIZE).unwrap();
            assert_eq!(parsed.num_nodes(), 2);
            assert_eq!(
                parsed.distances().collect::<Vec<_>>(),
                [(0, 1, 17), (1, 0, 29)]
            );
        }
        assert!(emit(vec![get_table(b"SRAT")], &params, false, 2).is_ok());
        let error = emit(vec![get_table(b"APIC")], &params, false, 2).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<Error>(),
            Some(Error::Slit(_))
        ));
        assert!(emit(vec![], &params, false, 2).is_err());
    }

    /// Ignores all host overrides for isolated VMs and emits no raw PPTT.
    #[test]
    fn isolated_output_ignores_get_and_never_emits_raw_pptt() {
        // Even non-isolated parameters must not pass through the isolated output guard.
        let params = runtime(&slit_bytes(), &table(b"PPTT", &[]), false).unwrap();
        let get = vec![
            vec![0],
            get_table(b"APIC"),
            get_table(b"APIC"),
            get_table(b"SRAT"),
            get_table(b"SRAT"),
            get_table(b"SLIT"),
            get_table(b"SLIT"),
            get_table(b"PPTT"),
        ];
        let output = emit(get, &params, true, 0).unwrap();
        assert_eq!(signatures(&output), [*b"APIC", *b"SRAT", *b"SLIT"]);
        assert_eq!(&output[2][10..24], b"HVLITEHVLITETB");
        assert_ne!(output[2], slit_bytes());
        let isolated = runtime(&slit_bytes(), &[], true).unwrap();
        assert_eq!(emit(vec![], &isolated, true, 0).unwrap(), output);
    }

    /// Omits SLIT and PPTT when IGVM supplies neither table.
    #[test]
    fn absent_igvm_tables_emit_only_required_local_tables() {
        let params = runtime(&[0; 36], &[0; 36], false).unwrap();
        let output = emit(vec![], &params, false, 0).unwrap();
        assert_eq!(signatures(&output), [*b"APIC", *b"SRAT"]);
        let get = vec![get_table(b"APIC"), get_table(b"SRAT")];
        assert_eq!(emit(get.clone(), &params, false, 0).unwrap(), get);
    }

    /// Checks all three sources of domains used to build a local SRAT.
    #[test]
    fn validates_all_local_srat_domains() {
        let info = SlitInfo {
            num_nodes: 2,
            distances: vec![],
        };
        for (vp_domain, memory_domain, initiator_domain) in [
            (2, 0, 0),
            (0, 2, 0),
            (0, 0, 2),
            (u32::MAX, 0, 0),
            (0, u32::MAX, 0),
            (0, 0, u32::MAX),
            (1, 1, 1),
        ] {
            let mut topology = TopologyBuilder::new_x86().build(1).unwrap();
            topology.set_vnodes(&[vp_domain]);
            let memory = MemoryLayout::new_from_ranges(
                &[MemoryRangeWithNode {
                    range: MemoryRange::new(0..4096),
                    vnode: memory_domain,
                }],
                &[],
            )
            .unwrap();
            let initiators = [GenericInitiator {
                segment: 0,
                bus: 0,
                device: 0,
                function: 0,
                vnode: initiator_domain,
            }];
            let result = validate_slit_srat_domains(&info, &topology, &memory, &initiators);
            if vp_domain < 2 && memory_domain < 2 && initiator_domain < 2 {
                assert_eq!(result, Ok(()));
            } else {
                let expected_domain = [vp_domain, memory_domain, initiator_domain]
                    .into_iter()
                    .find(|&domain| domain >= 2)
                    .unwrap();
                assert_eq!(
                    result,
                    Err(SlitValidationError::SratDomain {
                        domain: expected_domain,
                        num_nodes: 2,
                    })
                );
            }
        }
    }

    /// Rejects matrix indices and distances before SLIT construction.
    #[test]
    fn rejects_bad_slit_indices_and_distances() {
        for distances in [
            vec![(0, 2, 20)],
            vec![(2, 0, 20)],
            vec![(u32::MAX, 0, 20)],
            vec![(0, u32::MAX, 20)],
        ] {
            let info = SlitInfo {
                num_nodes: 2,
                distances,
            };
            assert!(matches!(
                validate_slit_info(&info, vtl2_config::SLIT_MAX_SIZE),
                Err(SlitValidationError::Index { .. })
            ));
        }
        for distances in [
            vec![(0, 0, 9)],
            vec![(0, 0, 11)],
            vec![(1, 1, 255)],
            vec![(0, 1, 0)],
            vec![(1, 0, 9)],
        ] {
            let info = SlitInfo {
                num_nodes: 2,
                distances,
            };
            assert!(matches!(
                validate_slit_info(&info, vtl2_config::SLIT_MAX_SIZE),
                Err(SlitValidationError::Distance { .. })
            ));
        }
    }

    /// Checks invalid counts, encoded length overflow, and exact size limits.
    #[test]
    fn rejects_invalid_slit_sizes_before_allocation() {
        for num_nodes in [0, usize::MAX] {
            let info = SlitInfo {
                num_nodes,
                distances: vec![],
            };
            let expected = if num_nodes == 0 || u32::try_from(num_nodes).is_err() {
                SlitValidationError::Localities(num_nodes)
            } else {
                SlitValidationError::SizeOverflow(num_nodes)
            };
            assert_eq!(validate_slit_info(&info, usize::MAX), Err(expected));
        }
        for (num_nodes, table_size) in [
            (65536, 4_294_967_340u64),
            (u32::MAX as usize, u64::from(u32::MAX).pow(2) + 44),
        ] {
            let info = SlitInfo {
                num_nodes,
                distances: vec![],
            };
            let expected = match usize::try_from(table_size) {
                Ok(actual) => SlitValidationError::TooLarge {
                    actual,
                    limit: u32::MAX as usize,
                },
                Err(_) => SlitValidationError::SizeOverflow(num_nodes),
            };
            assert_eq!(validate_slit_info(&info, usize::MAX), Err(expected));
        }
        let info = SlitInfo {
            num_nodes: 65535,
            distances: vec![],
        };
        assert_eq!(validate_slit_info(&info, u32::MAX as usize), Ok(()));
        let info = SlitInfo {
            num_nodes: 2,
            distances: vec![],
        };
        assert_eq!(
            validate_slit_info(&info, 47),
            Err(SlitValidationError::TooLarge {
                actual: 48,
                limit: 47,
            })
        );
        assert_eq!(validate_slit_info(&info, 48), Ok(()));
        assert_eq!(validate_slit_info(&info, usize::MAX), Ok(()));
    }
}
