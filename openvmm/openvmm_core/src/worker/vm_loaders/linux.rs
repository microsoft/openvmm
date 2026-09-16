// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::device_tree::DeviceTreeBuilder;
use super::device_tree::DeviceTreeError;
use crate::worker::memory_layout::ChipsetMmioRanges;
use guestmem::GuestMemory;
use loader::importer::Aarch64Register;
use loader::importer::X86Register;
use loader::linux::InitrdAddressType;
use loader::linux::InitrdConfig;
use memory_range::MemoryRange;
use std::ffi::CString;
use std::io::Seek;
use thiserror::Error;
use vm_loader::InitialLoad;
use vm_loader::Loader;
use vm_topology::memory::MemoryLayout;
use vm_topology::pcie::PcieHostBridge;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::aarch64::Aarch64Topology;
use vmm_core_defs::uart::UartId;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read initrd file")]
    InitRd(#[source] std::io::Error),
    #[error("linux loader error")]
    Loader(#[source] loader::linux::Error),
    #[error("device tree error")]
    Dt(#[source] DeviceTreeError),
    #[error("failed to write EFI/ACPI tables to guest memory")]
    Efi(#[source] guestmem::GuestMemoryError),
    #[error("failed to finalize SNP VMSA")]
    SnpVmsa(#[source] anyhow::Error),
}

struct Aarch64EfiInfo {
    systab_addr: u64,
    mmap_addr: u64,
    mmap_size: u32,
    mmap_desc_size: u32,
    mmap_desc_ver: u32,
}

#[derive(Debug)]
pub struct KernelConfig<'a> {
    pub kernel: &'a std::fs::File,
    pub initrd: &'a Option<std::fs::File>,
    pub cmdline: &'a str,
    pub mem_layout: &'a MemoryLayout,
    pub isolation: KernelIsolationConfig,
    pub smbios: &'a openvmm_defs::config::SmbiosConfig,
}

#[derive(Debug, Clone, Copy)]
pub enum KernelIsolationConfig {
    None,
    #[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
    Snp(SnpKernelConfig),
}

#[derive(Debug, Clone, Copy)]
pub struct SnpKernelConfig {
    pub c_bit: u8,
    pub restricted_injection: bool,
}

// Bring-up hack for SNP Linux direct boot. Without a bootshim or firmware to
// accept memory after launch, every RAM page must be added to the initial SNP
// launch context. This makes launch extremely slow and should be removed once
// SNP boots exclusively through IGVM, or direct boot can accept the remaining
// RAM after launch instead of pre-accepting it here.
fn complete_snp_direct_ram_imports(
    page_imports: &mut Vec<virt::InitialPageImport>,
    ram_ranges: impl IntoIterator<Item = MemoryRange>,
) {
    let mut imported_ranges: Vec<_> = page_imports.iter().map(|page| page.range).collect();
    imported_ranges.sort_by_key(|range| (range.start(), range.end()));

    for ram_range in ram_ranges {
        let mut cursor = ram_range.start();
        for imported_range in &imported_ranges {
            let start = imported_range.start().max(ram_range.start());
            let end = imported_range.end().min(ram_range.end());
            if start >= end {
                continue;
            }
            if cursor < start {
                page_imports.push(virt::InitialPageImport {
                    range: MemoryRange::new(cursor..start),
                    import_type: virt::InitialPageImportType::Normal,
                    tag: "linux-snp-direct-ram",
                });
            }
            cursor = cursor.max(end);
        }
        if cursor < ram_range.end() {
            page_imports.push(virt::InitialPageImport {
                range: MemoryRange::new(cursor..ram_range.end()),
                import_type: virt::InitialPageImportType::Normal,
                tag: "linux-snp-direct-ram",
            });
        }
    }
}

/// Merges the owned SMBIOS override config into the borrowed table view the
/// builder consumes, substituting the `loader` crate's default strings for any
/// unset (`None`) override.
fn smbios_tables_from_config(
    config: &openvmm_defs::config::SmbiosConfig,
) -> loader::smbios::SmbiosTables<'_> {
    use loader::smbios;

    // Destructure fully so new fields must be wired into the table view.
    let openvmm_defs::config::SmbiosConfig { bios, system } = config;
    let openvmm_defs::config::SmbiosBiosOverrides {
        vendor,
        version: bios_version,
        release_date,
        release,
    } = bios;
    let openvmm_defs::config::SmbiosSystemOverrides {
        manufacturer,
        product_name,
        version: system_version,
        serial_number,
        sku_number,
        family,
        uuid,
    } = system;

    const DEFAULT_BIOS_VENDOR: &str = "OpenVMM";
    const DEFAULT_BIOS_VERSION: &str = "OpenVMM Direct";
    const DEFAULT_BIOS_RELEASE_DATE: &str = "06/19/2026";
    const DEFAULT_BIOS_MAJOR: u8 = 0;
    const DEFAULT_BIOS_MINOR: u8 = 0;
    const DEFAULT_MANUFACTURER: &str = "OpenVMM";
    const DEFAULT_PRODUCT_NAME: &str = "OpenVMM Virtual Machine";

    smbios::SmbiosTables {
        bios: smbios::SmbiosBiosInfo {
            vendor: vendor.as_deref().unwrap_or(DEFAULT_BIOS_VENDOR),
            version: bios_version.as_deref().unwrap_or(DEFAULT_BIOS_VERSION),
            release_date: release_date.as_deref().unwrap_or(DEFAULT_BIOS_RELEASE_DATE),
            major: release.map_or(DEFAULT_BIOS_MAJOR, |(major, _)| major),
            minor: release.map_or(DEFAULT_BIOS_MINOR, |(_, minor)| minor),
        },
        system: smbios::SmbiosSystemInfo {
            manufacturer: manufacturer.as_deref().unwrap_or(DEFAULT_MANUFACTURER),
            product_name: product_name.as_deref().unwrap_or(DEFAULT_PRODUCT_NAME),
            version: system_version.as_deref().unwrap_or(""),
            serial_number: serial_number.as_deref().unwrap_or(""),
            sku_number: sku_number.as_deref().unwrap_or(""),
            family: family.as_deref().unwrap_or(""),
            // SMBIOS (>= 2.6) stores the Type 1 UUID's first three fields
            // little-endian, which is exactly the in-memory byte layout of our
            // `Guid` type, so its raw bytes go in directly with no swap. The
            // UEFI boot path uses the same VM BIOS GUID, so a guest reports the
            // same `product_uuid` whether booted via UEFI or direct boot.
            uuid: (*uuid).into(),
        },
    }
}

#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
pub fn load_linux_x86(
    cfg: &KernelConfig<'_>,
    gm: &GuestMemory,
    caps: &virt::x86::X86PartitionCapabilities,
    bsp: &vm_topology::processor::x86::X86VpInfo,
    acpi_at_gpa: impl FnOnce(u64) -> loader::linux::AcpiTables,
) -> Result<InitialLoad<X86Register>, Error> {
    let mut kernel_file = cfg.kernel;

    let (mut initrd_reader, initrd_size) = if let Some(mut initrd_file) = cfg.initrd.as_ref() {
        initrd_file.rewind().map_err(Error::InitRd)?;
        let size = initrd_file
            .seek(std::io::SeekFrom::End(0))
            .map_err(Error::InitRd)?;
        (Some(initrd_file), size)
    } else {
        (None, 0)
    };
    let initrd_config = initrd_reader.as_mut().map(|r| InitrdConfig {
        initrd_address: InitrdAddressType::AfterKernel,
        initrd: r,
        size: initrd_size,
    });

    let cmdline = CString::new(cfg.cmdline).unwrap();
    let snp = match cfg.isolation {
        KernelIsolationConfig::None => None,
        KernelIsolationConfig::Snp(snp) => Some(snp),
    };
    let snp_boot = snp.map(|snp| loader::linux::SnpBootConfig { c_bit: snp.c_bit });

    let mut loader = Loader::new(gm.clone(), cfg.mem_layout, hvdef::Vtl::Vtl0);

    // The loader owns the sub-1 MB layout; we supply only the kernel, command
    // line, an ACPI builder, and the configured SMBIOS identity.
    loader::linux::load_x86(
        &mut loader,
        &mut kernel_file,
        initrd_config,
        &cmdline,
        cfg.mem_layout,
        acpi_at_gpa,
        Some(smbios_tables_from_config(cfg.smbios)),
        snp_boot,
    )
    .map_err(Error::Loader)?;

    if let Some(snp) = snp {
        loader
            .finalize_snp_vmsa(
                caps,
                bsp,
                virt::x86::snp::SnpVmsaConfig {
                    restricted_injection: snp.restricted_injection,
                },
            )
            .map_err(Error::SnpVmsa)?;
    }

    let InitialLoad {
        regs,
        mut page_imports,
    } = loader.initial_regs_and_page_imports();
    if snp.is_some() {
        complete_snp_direct_ram_imports(
            &mut page_imports,
            cfg.mem_layout.ram().iter().map(|range| range.range),
        );
    }

    Ok(InitialLoad { regs, page_imports })
}

/// Write synthesized EFI and ACPI structures into guest memory.
///
/// On ARM64, the Linux kernel can discover devices via ACPI instead of a
/// device tree, but it still needs to enter via the EFI stub to find the
/// RSDP. We synthesize:
///   - An `EFI_SYSTEM_TABLE` pointing to an ACPI 2.0 configuration table
///     entry (the RSDP) and an RT Properties table (advertising no runtime
///     services).
///   - An EFI memory map describing the metadata, ACPI tables, and
///     conventional RAM regions.
///   - The ACPI tables themselves (RSDP, XSDT, FADT, MADT, GTDT, DSDT, etc.).
///
/// The companion [`build_stub_dt`] function then builds a minimal device tree
/// whose `/chosen` node carries `linux,uefi-system-table` and the memory map
/// pointers so that the kernel's EFI stub can locate these structures.
fn write_efi_and_acpi_tables(
    gm: &GuestMemory,
    efi_base: u64,
    rsdp_addr: u64,
    mem_layout: &MemoryLayout,
    acpi_tables: &vmm_core::acpi_builder::BuiltAcpiTables,
    smbios: &openvmm_defs::config::SmbiosConfig,
) -> Result<Aarch64EfiInfo, Error> {
    use memory_range::MemoryRange;
    use uefi_specs::uefi::boot::ACPI_20_TABLE_GUID;
    use uefi_specs::uefi::boot::EFI_2_70_SYSTEM_TABLE_REVISION;
    use uefi_specs::uefi::boot::EFI_MEMORY_DESCRIPTOR_VERSION;
    use uefi_specs::uefi::boot::EFI_MEMORY_WB;
    use uefi_specs::uefi::boot::EFI_RT_PROPERTIES_TABLE_GUID;
    use uefi_specs::uefi::boot::EFI_SYSTEM_TABLE_SIGNATURE;
    use uefi_specs::uefi::boot::EfiMemoryDescriptor;
    use uefi_specs::uefi::boot::EfiMemoryType;
    use uefi_specs::uefi::boot::EfiRtPropertiesTable;
    use uefi_specs::uefi::boot::EfiSystemTable;
    use uefi_specs::uefi::boot::SMBIOS3_TABLE_GUID;

    // Helper to align a value up to the given power-of-two alignment.
    fn align_up(val: u64, align: u64) -> u64 {
        (val + align - 1) & !(align - 1)
    }

    // --- ACPI tables ---
    let tables_addr = rsdp_addr + 0x1000;
    gm.write_at(rsdp_addr, &acpi_tables.rsdp)
        .map_err(Error::Efi)?;
    gm.write_at(tables_addr, &acpi_tables.tables)
        .map_err(Error::Efi)?;

    // --- EFI metadata (page 1): systab, config table, vendor, rt props ---
    // Page 0 is reserved for the memory map (written last).
    let mut cursor = efi_base + 0x1000;

    // EFI System Table
    let systab_addr = cursor;
    cursor += size_of::<EfiSystemTable>() as u64;

    // Configuration table entries (24 bytes each: 16-byte GUID + 8-byte pointer)
    const CONFIG_ENTRY_SIZE: u64 = 24;
    let num_config_entries: u64 = 3;
    let config_table_addr = cursor;
    cursor += num_config_entries * CONFIG_ENTRY_SIZE;

    // Firmware vendor string — NUL-terminated UTF-16LE
    let fw_vendor_addr = cursor;
    let fw_vendor: Vec<u8> = "OpenVMM\0"
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    cursor += fw_vendor.len() as u64;
    cursor = align_up(cursor, 8);

    // EFI RT Properties Table — tells the OS no runtime services are available.
    let rt_props_addr = cursor;
    let rt_props = EfiRtPropertiesTable::NONE_SUPPORTED;
    cursor += size_of::<EfiRtPropertiesTable>() as u64;

    // SMBIOS — unlike x86 (which brute-force scans the F-segment for the
    // `_SM3_` anchor), the aarch64 kernel discovers DMI only via the SMBIOS3
    // EFI configuration-table entry. Reserve the entry point and structure
    // table from the metadata page (16-byte aligned) and build them with the
    // shared arch-neutral table builder.
    cursor = align_up(cursor, 16);
    let smbios_ep_addr = cursor;
    cursor += loader::smbios::ENTRY_POINT_SIZE as u64;
    cursor = align_up(cursor, 16);
    let smbios_table_addr = cursor;
    let smbios = loader::smbios::build(&smbios_tables_from_config(smbios), smbios_table_addr);
    cursor += smbios.structure_table.len() as u64;

    // Compute how many pages the metadata region spans.
    let metadata_end = align_up(cursor, 0x1000);
    let metadata_pages = (metadata_end - efi_base) / 0x1000;
    assert!(
        cursor <= rsdp_addr,
        "EFI metadata ({cursor:#x}) overflows into ACPI tables region ({rsdp_addr:#x})",
    );

    // Now write everything.
    gm.write_at(rt_props_addr, rt_props.as_bytes())
        .map_err(Error::Efi)?;

    gm.write_at(smbios_ep_addr, &smbios.entry_point)
        .map_err(Error::Efi)?;
    gm.write_at(smbios_table_addr, &smbios.structure_table)
        .map_err(Error::Efi)?;

    let mut config_entries = [0u8; 72];
    config_entries[0..16].copy_from_slice(ACPI_20_TABLE_GUID.as_bytes());
    config_entries[16..24].copy_from_slice(&rsdp_addr.to_le_bytes());
    config_entries[24..40].copy_from_slice(EFI_RT_PROPERTIES_TABLE_GUID.as_bytes());
    config_entries[40..48].copy_from_slice(&rt_props_addr.to_le_bytes());
    config_entries[48..64].copy_from_slice(SMBIOS3_TABLE_GUID.as_bytes());
    config_entries[64..72].copy_from_slice(&smbios_ep_addr.to_le_bytes());
    gm.write_at(config_table_addr, &config_entries)
        .map_err(Error::Efi)?;

    gm.write_at(fw_vendor_addr, &fw_vendor)
        .map_err(Error::Efi)?;

    let mut systab = EfiSystemTable {
        signature: EFI_SYSTEM_TABLE_SIGNATURE,
        revision: EFI_2_70_SYSTEM_TABLE_REVISION,
        header_size: size_of::<EfiSystemTable>() as u32,
        firmware_vendor: fw_vendor_addr,
        firmware_revision: 1,
        number_of_table_entries: num_config_entries,
        configuration_table: config_table_addr,
        ..Default::default()
    };
    // UEFI spec 4.2: CRC32 is computed over header_size bytes with crc32 zeroed.
    systab.crc32 = crc32fast::hash(systab.as_bytes());
    gm.write_at(systab_addr, systab.as_bytes())
        .map_err(Error::Efi)?;

    // --- Memory map (page 0) ---
    let mut mmap_entries: Vec<EfiMemoryDescriptor> = Vec::new();

    // EFI metadata region
    mmap_entries.push(EfiMemoryDescriptor {
        typ: EfiMemoryType::EFI_BOOT_SERVICES_DATA,
        _pad: 0,
        physical_start: efi_base,
        virtual_start: 0,
        number_of_pages: metadata_pages,
        attribute: EFI_MEMORY_WB,
    });

    // ACPI tables region
    let acpi_region_pages = {
        let total = 0x1000 + acpi_tables.tables.len() as u64;
        total.div_ceil(0x1000)
    };
    mmap_entries.push(EfiMemoryDescriptor {
        typ: EfiMemoryType::EFI_ACPI_RECLAIM_MEMORY,
        _pad: 0,
        physical_start: rsdp_addr,
        virtual_start: 0,
        number_of_pages: acpi_region_pages,
        attribute: EFI_MEMORY_WB,
    });

    // Conventional memory — one entry per RAM range, excluding the
    // EFI/ACPI reserved region to avoid overlapping memory map entries.
    let reserved_start = efi_base;
    let reserved_end = align_up(rsdp_addr + 0x1000 + acpi_tables.tables.len() as u64, 0x1000);
    let reserved = [MemoryRange::new(reserved_start..reserved_end)];
    for range in memory_range::subtract_ranges(mem_layout.ram().iter().map(|r| r.range), reserved) {
        mmap_entries.push(EfiMemoryDescriptor {
            typ: EfiMemoryType::EFI_CONVENTIONAL_MEMORY,
            _pad: 0,
            physical_start: range.start(),
            virtual_start: 0,
            number_of_pages: range.len() / 0x1000,
            attribute: EFI_MEMORY_WB,
        });
    }

    let mmap_addr = efi_base;
    let mmap_bytes: Vec<u8> = mmap_entries
        .iter()
        .flat_map(|e| e.as_bytes())
        .copied()
        .collect();
    let mmap_size = mmap_bytes.len() as u32;

    gm.write_at(mmap_addr, &mmap_bytes).map_err(Error::Efi)?;

    Ok(Aarch64EfiInfo {
        systab_addr,
        mmap_addr,
        mmap_size,
        mmap_desc_size: size_of::<EfiMemoryDescriptor>() as u32,
        mmap_desc_ver: EFI_MEMORY_DESCRIPTOR_VERSION,
    })
}

/// Build a "stub" device tree for ACPI-mode ARM64 direct boot.
///
/// Unlike the full device tree built by [`DeviceTreeBuilder`], this DT contains no
/// hardware descriptions — no CPU nodes, no GIC, no timer, no devices.
/// Its only purpose is a `/chosen` node that tells the Linux EFI stub
/// where to find the EFI system table and memory map written by
/// [`write_efi_and_acpi_tables`]. The kernel then uses those EFI
/// structures to locate the ACPI RSDP and discovers all hardware through
/// ACPI tables instead of DT nodes.
fn build_stub_dt(
    cmdline: &str,
    initrd_start: u64,
    initrd_end: u64,
    efi_info: &Aarch64EfiInfo,
) -> Result<Vec<u8>, fdt::builder::Error> {
    let mut buffer = vec![0u8; 0x4000];

    let builder_config = fdt::builder::BuilderConfig {
        blob_buffer: &mut buffer,
        string_table_cap: 256,
        memory_reservations: &[],
    };
    let mut builder = fdt::builder::Builder::new(builder_config)?;
    let p_address_cells = builder.add_string("#address-cells")?;
    let p_size_cells = builder.add_string("#size-cells")?;
    let p_bootargs = builder.add_string("bootargs")?;
    let p_initrd_start = builder.add_string("linux,initrd-start")?;
    let p_initrd_end = builder.add_string("linux,initrd-end")?;
    let p_uefi_system_table = builder.add_string("linux,uefi-system-table")?;
    let p_uefi_mmap_start = builder.add_string("linux,uefi-mmap-start")?;
    let p_uefi_mmap_size = builder.add_string("linux,uefi-mmap-size")?;
    let p_uefi_mmap_desc_size = builder.add_string("linux,uefi-mmap-desc-size")?;
    let p_uefi_mmap_desc_ver = builder.add_string("linux,uefi-mmap-desc-ver")?;
    let p_uefi_secure_boot = builder.add_string("linux,uefi-secure-boot")?;

    let root_builder = builder
        .start_node("")?
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?;

    let chosen = root_builder
        .start_node("chosen")?
        .add_str(p_bootargs, cmdline)?
        .add_u64(p_initrd_start, initrd_start)?
        .add_u64(p_initrd_end, initrd_end)?
        .add_u64(p_uefi_system_table, efi_info.systab_addr)?
        .add_u64(p_uefi_mmap_start, efi_info.mmap_addr)?
        .add_u32(p_uefi_mmap_size, efi_info.mmap_size)?
        .add_u32(p_uefi_mmap_desc_size, efi_info.mmap_desc_size)?
        .add_u32(p_uefi_mmap_desc_ver, efi_info.mmap_desc_ver)?
        // The Ubuntu kernel's EFI stub sets `linux,uefi-secure-boot` in the
        // handoff FDT, and `efi_get_fdt_params()` then treats it as a required
        // property. If it is absent, the kernel aborts the entire EFI handoff
        // and never installs the memory map; because this stub DT has no
        // `/memory` node, memblock ends up empty and the kernel panics with
        // "Failed to allocate page table page" during paging_init. Emit it
        // (0 = secure boot disabled) so those kernels boot. Mainline kernels
        // ignore this property.
        .add_u32(p_uefi_secure_boot, 0)?;

    let root_builder = chosen.end_node()?;

    let boot_cpu_id = 0;
    let dt_size = root_builder.end_node()?.build(boot_cpu_id)?;
    buffer.truncate(dt_size);

    Ok(buffer)
}

#[cfg_attr(not(guest_arch = "aarch64"), expect(dead_code))]
pub fn load_linux_arm64(
    cfg: &KernelConfig<'_>,
    gm: &GuestMemory,
    uarts: &[UartId],
    console: Option<UartId>,
    processor_topology: &ProcessorTopology<Aarch64Topology>,
    pcie_host_bridges: &[PcieHostBridge],
    smmu_configs: &[vmm_core::acpi_builder::AcpiSmmuConfig],
    chipset_mmio: &ChipsetMmioRanges,
    build_acpi: Option<impl FnOnce(u64) -> vmm_core::acpi_builder::BuiltAcpiTables>,
) -> Result<InitialLoad<Aarch64Register>, Error> {
    let mut loader = Loader::new(gm.clone(), cfg.mem_layout, hvdef::Vtl::Vtl0);
    let mut kernel_file = cfg.kernel;

    let (mut initrd_reader, initrd_size) = if let Some(mut initrd_file) = cfg.initrd.as_ref() {
        initrd_file.rewind().map_err(Error::InitRd)?;
        let size = initrd_file
            .seek(std::io::SeekFrom::End(0))
            .map_err(Error::InitRd)?;
        (Some(initrd_file), size)
    } else {
        (None, 0)
    };

    // Data dependencies:
    // - DeviceTree carries the start address of the initrd.
    // - The linux loader loads the kernel, the initrd at the said address, and
    //   the device tree into the guest memory.
    //
    // Place the initrd at the bottom of guest memory + 16MB, and set the
    // minimum kernel address above it, aligned to the next 2MB boundary.
    let mem_start = cfg
        .mem_layout
        .ram()
        .first()
        .expect("must be at least one ram range")
        .range
        .start();
    const INITRD_OFFSET: u64 = 16 << 20; // 16 MB
    let initrd_start: u64 = mem_start + INITRD_OFFSET;
    let initrd_end: u64 = initrd_start + initrd_size;
    // Align the kernel to 2MB
    let kernel_minimum_start_address: u64 = (initrd_end + 0x1fffff) & !0x1fffff;

    let device_tree = if let Some(build_acpi) = build_acpi {
        // ACPI mode: write EFI + ACPI tables into guest memory, then build a
        // minimal "stub" DT that points the kernel's EFI stub at them. The
        // kernel discovers all devices through ACPI, not the DT.
        const EFI_OFFSET: u64 = 0x0080_0000; // 8 MB
        const ACPI_TABLES_OFFSET: u64 = 0x2000;
        const { assert!(EFI_OFFSET < INITRD_OFFSET) };
        let rsdp_addr = mem_start + EFI_OFFSET + ACPI_TABLES_OFFSET;
        let acpi_tables = build_acpi(rsdp_addr);
        let efi_info = write_efi_and_acpi_tables(
            gm,
            mem_start + EFI_OFFSET,
            rsdp_addr,
            cfg.mem_layout,
            &acpi_tables,
            cfg.smbios,
        )?;
        build_stub_dt(cfg.cmdline, initrd_start, initrd_end, &efi_info)
            .map_err(|e| Error::Dt(e.into()))?
    } else {
        DeviceTreeBuilder::new(
            processor_topology,
            cfg.mem_layout.ram(),
            uarts,
            pcie_host_bridges,
        )
        .with_linux_direct(chipset_mmio.low, chipset_mmio.high)
        .with_linux_initrd(Some((initrd_start, initrd_end)))
        .with_linux_smmus(smmu_configs)
        .with_command_line(cfg.cmdline)
        .with_console(console)
        .with_capacity(0x200000)
        .finish()
        .map_err(Error::Dt)?
    };

    let initrd_config = initrd_reader.as_mut().map(|r| InitrdConfig {
        initrd_address: InitrdAddressType::Address(initrd_start),
        initrd: r,
        size: initrd_size,
    });

    let load_info = loader::linux::load_kernel_and_initrd_arm64(
        &mut loader,
        &mut kernel_file,
        kernel_minimum_start_address,
        initrd_config,
        Some(&device_tree),
    )
    .map_err(Error::Loader)?;

    // Set the registers separately so they won't conflict with the UEFI boot when
    // `load_kernel_and_initrd_arm64` is used for VTL2 direct kernel boot.
    loader::linux::set_direct_boot_registers_arm64(&mut loader, &load_info)
        .map_err(Error::Loader)?;

    Ok(loader.initial_regs_and_page_imports())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn completes_snp_direct_ram_imports() {
        let mut page_imports = vec![virt::InitialPageImport {
            range: MemoryRange::new(0x2000..0x4000),
            import_type: virt::InitialPageImportType::Secrets,
            tag: "loader",
        }];

        complete_snp_direct_ram_imports(
            &mut page_imports,
            [
                MemoryRange::new(0x1000..0x5000),
                MemoryRange::new(0x8000..0xa000),
            ],
        );

        let completed_ranges: Vec<_> = page_imports
            .iter()
            .filter(|page| page.tag == "linux-snp-direct-ram")
            .map(|page| page.range)
            .collect();
        assert_eq!(
            completed_ranges,
            [
                MemoryRange::new(0x1000..0x2000),
                MemoryRange::new(0x4000..0x5000),
                MemoryRange::new(0x8000..0xa000),
            ]
        );
        assert_eq!(
            page_imports[0].import_type,
            virt::InitialPageImportType::Secrets
        );
    }
}
