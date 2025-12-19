// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functionality to prepare VTL0 to run.

use self::vtl2_config::RuntimeParameters;
use crate::loader::vtl0_config::LinuxInfo;
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
    #[error("invalid acpi table: unknown header signature {0:?}")]
    InvalidAcpiTableSignature([u8; 4]),
    #[cfg(guest_arch = "x86_64")]
    #[error("acpi tables require at least two mmio ranges")]
    UnsupportedMmio,
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
}

/// Load VTL0 based on measured config. Returns any VP state that should be set.
pub fn load(
    gm: &GuestMemory,
    mem_layout: &MemoryLayout,
    processor_topology: &ProcessorTopology,
    vtl0_memory_map: &[(MemoryRangeWithNode, MemoryMapEntryType)],
    runtime_params: &RuntimeParameters,
    load_kind: LoadKind,
    vtl0_info: vtl0_config::MeasuredVtl0Info,
    platform_config: &DevicePlatformSettings,
    config: Config,
    caps: &virt::PartitionCapabilities,
    isolated: bool,
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
                platform_config,
                caps,
                isolated,
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

            load_linux(LoadLinuxParams {
                gm,
                mem_layout,
                processor_topology,
                platform_config,
                kernel_range: *kernel_range,
                kernel_entrypoint: *kernel_entrypoint,
                initrd: *initrd,
                command_line,
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
    /// The region of memory used by the kernel.
    kernel_range: MemoryRange,
    /// The entrypoint of the kernel.
    kernel_entrypoint: u64,
    /// The (base address, size in bytes) of the initrd.
    initrd: Option<(u64, u64)>,
    /// The command line to pass to the kernel.
    command_line: CString,
}

/// Load Linux into VTL0.
#[cfg(guest_arch = "x86_64")]
fn load_linux(params: LoadLinuxParams<'_>) -> Result<VpContext, Error> {
    const GDT_BASE: u64 = 0x1000;
    const CR3_BASE: u64 = 0x4000;
    const ZERO_PAGE_BASE: u64 = 0x2000;
    const CMDLINE_BASE: u64 = 0x3000;
    const ACPI_BASE: u64 = 0xe0000;

    let LoadLinuxParams {
        gm,
        mem_layout,
        processor_topology,
        platform_config,
        kernel_range,
        kernel_entrypoint,
        initrd,
        command_line,
    } = params;

    let cmdline_config = loader::linux::CommandLineConfig {
        address: CMDLINE_BASE,
        cmdline: &command_line,
    };

    let acpi_builder = AcpiTablesBuilder {
        processor_topology,
        mem_layout,
        cache_topology: None,
        pcie_host_bridges: &vec![],
        with_ioapic: true, // underhill always runs with ioapic
        with_pic: false,
        with_pit: false,
        with_psp: platform_config.general.psp_enabled,
        pm_base: crate::worker::PM_BASE,
        acpi_irq: crate::worker::SYSTEM_IRQ_ACPI,
    };

    if mem_layout.mmio().len() < 2 {
        return Err(Error::UnsupportedMmio);
    }

    let acpi_tables = acpi_builder.build_acpi_tables(ACPI_BASE, |mem_layout, dsdt| {
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

        dsdt.add_mmio_module(mem_layout.mmio()[0], mem_layout.mmio()[1]);
        // TODO: change this once PCI is running in underhill
        dsdt.add_vmbus(false);
        dsdt.add_rtc();
    });
    let acpi_len = acpi_tables.tables.len() + 0x1000;

    let acpi_config = loader::linux::AcpiConfig {
        rdsp_address: ACPI_BASE,
        rdsp: &acpi_tables.rdsp,
        tables_address: ACPI_BASE + 0x1000,
        tables: &acpi_tables.tables,
    };

    let register_config = loader::linux::RegisterConfig {
        gdt_address: GDT_BASE,
        page_table_address: CR3_BASE,
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

    let zero_page_config = loader::linux::ZeroPageConfig {
        address: ZERO_PAGE_BASE,
        mem_layout,
        acpi_base_address: ACPI_BASE,
        acpi_len,
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
    };

    loader::linux::load_config(
        &mut loader,
        &load_info,
        cmdline_config,
        zero_page_config,
        acpi_config,
        register_config,
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
    platform_config: &DevicePlatformSettings,
    caps: &virt::PartitionCapabilities,
    isolated: bool,
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

    // We will generate these tables unless trusted tables are passed via DevicePlatformSettings
    let mut build_madt = true;
    let mut build_srat = true;

    // ACPI tables that come from the DevicePlatformSettings
    // We can only trust these tables from the host if this is not an isolated VM
    if !isolated {
        for table in &platform_config.acpi_tables {
            let header = acpi_spec::Header::ref_from_prefix(table)
                .map_err(|_| Error::InvalidAcpiTableLength)? // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                .0;
            match &header.signature {
                b"APIC" => {
                    build_madt = false;
                    cfg.add_raw(config::BlobStructureType::Madt, table)
                }
                b"HMAT" => cfg.add_raw(config::BlobStructureType::Hmat, table),
                b"IORT" => cfg.add_raw(config::BlobStructureType::Iort, table),
                b"MCFG" => cfg.add_raw(config::BlobStructureType::Mcfg, table),
                b"SRAT" => {
                    build_srat = false;
                    cfg.add_raw(config::BlobStructureType::Srat, table)
                }
                b"SSDT" => cfg.add_raw(config::BlobStructureType::Ssdt, table),
                _ => return Err(Error::InvalidAcpiTableSignature(header.signature)),
            };
        }
    }

    // - Data that comes from the IGVM parameters

    if build_madt || build_srat {
        let acpi_builder = AcpiTablesBuilder {
            processor_topology,
            mem_layout,
            cache_topology: None,
            pcie_host_bridges: &vec![],
            with_ioapic: cfg!(guest_arch = "x86_64"), // OpenHCL always runs with ioapic on x64
            with_pic: false,                          // uefi never runs with pic or pit
            with_pit: false,
            with_psp: platform_config.general.psp_enabled,
            pm_base: crate::worker::PM_BASE,
            acpi_irq: crate::worker::SYSTEM_IRQ_ACPI,
        };

        // Build the ACPI tables as specified.
        if build_madt {
            let madt = acpi_builder.build_madt();
            cfg.add_raw(config::BlobStructureType::Madt, &madt);
        }

        if build_srat {
            let srat = acpi_builder.build_srat();
            cfg.add_raw(config::BlobStructureType::Srat, &srat);
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
            mem_layout
                .mmio()
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

        if let Some(slit) = igvm_parameters.slit() {
            cfg.add_raw(config::BlobStructureType::Slit, slit);
        }

        // TODO: reconstruct this instead of getting it from the host.
        if let Some(pptt) = igvm_parameters.pptt() {
            cfg.add_raw(config::BlobStructureType::Pptt, pptt);
        }
    }

    cfg.add(&config::BiosInformation {
        bios_size_pages: (IMAGE_SIZE / HV_PAGE_SIZE) as u32,
        flags: platform_config.general.legacy_memory_map as u32,
    })
    .add(&config::BiosGuid(platform_config.general.bios_guid))
    .add_cstring(
        config::BlobStructureType::SmbiosSystemSerialNumber,
        &platform_config.smbios.serial_number,
    )
    .add_cstring(
        config::BlobStructureType::SmbiosBaseSerialNumber,
        &platform_config.smbios.base_board_serial_number,
    )
    .add_cstring(
        config::BlobStructureType::SmbiosChassisSerialNumber,
        &platform_config.smbios.chassis_serial_number,
    )
    .add_cstring(
        config::BlobStructureType::SmbiosChassisAssetTag,
        &platform_config.smbios.chassis_asset_tag,
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
        &platform_config.smbios.system_manufacturer,
    )
    .add_cstring(
        config::BlobStructureType::SmbiosSystemProductName,
        &platform_config.smbios.system_product_name,
    )
    .add_cstring(
        config::BlobStructureType::SmbiosSystemVersion,
        &platform_config.smbios.system_version,
    )
    .add_cstring(
        config::BlobStructureType::SmbiosSystemSkuNumber,
        &platform_config.smbios.system_sku_number,
    )
    .add_cstring(
        config::BlobStructureType::SmbiosSystemFamily,
        &platform_config.smbios.system_family,
    )
    .add_cstring(
        config::BlobStructureType::SmbiosBiosLockString,
        &platform_config.smbios.bios_lock_string,
    )
    .add_cstring(
        config::BlobStructureType::SmbiosMemoryDeviceSerialNumber,
        &platform_config.smbios.memory_device_serial_number,
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
        cfg.add(&config::Gic {
            gic_distributor_base: processor_topology.gic_distributor_base(),
            gic_redistributors_base: processor_topology.gic_redistributors_base(),
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
