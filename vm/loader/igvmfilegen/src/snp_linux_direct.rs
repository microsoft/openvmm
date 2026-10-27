// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generation strategy for a self-contained SNP Linux-direct IGVM.
//!
//! The image starts the SNP bootshim. The bootshim builds a minimal UEFI
//! environment and enters the kernel's EFI stub through its PE/COFF entry
//! point. The stub then loads the initrd, builds the memory map for Linux,
//! and sets up lazy acceptance for the memory that the image does not import.

use crate::file_loader::IgvmLoader;
use crate::file_loader::IgvmOutput;
use crate::file_loader::SnpLinuxDirectConfig;
use crate::vp_context_builder::snp::InjectionType;
use anyhow::Context;
use anyhow::ensure;
use igvm_defs::SnpPolicy;
use igvmfilegen_config::LinuxImage;
use igvmfilegen_config::ResourceType;
use igvmfilegen_config::Resources;
use igvmfilegen_config::SnpInjectionType;
use loader::importer::BootPageAcceptance;
use loader::importer::IgvmParameterType;
use loader::importer::ImageLoad;
use loader::importer::X86Register;
use loader::pe::PeImage;
use loader_defs::linux::SNP_BOOT_SHIM_ACPI_SIZE;
use loader_defs::linux::SNP_BOOT_SHIM_DT_SIZE;
use loader_defs::linux::SNP_BOOT_SHIM_EFI_STACK_SIZE;
use loader_defs::linux::SNP_BOOT_SHIM_HEAP_SIZE;
use loader_defs::linux::SNP_BOOT_SHIM_KERNEL_MIN_ADDRESS;
use loader_defs::linux::SNP_BOOT_SHIM_MAX_RANGES;
use loader_defs::linux::SNP_BOOT_SHIM_PARAMS_MAGIC;
use loader_defs::linux::SNP_BOOT_SHIM_PARAMS_VERSION;
use loader_defs::linux::SNP_BOOT_SHIM_PLATFORM_MAGIC;
use loader_defs::linux::SNP_BOOT_SHIM_PLATFORM_VERSION;
use loader_defs::linux::SnpBootShimParams;
use loader_defs::linux::SnpBootShimPlatformParams;
use loader_defs::linux::SnpBootShimRange;
use memory_range::MemoryRange;
use std::ffi::CStr;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const PAGE_SIZE: u64 = igvm_defs::PAGE_SIZE_4K;
/// KVM hardcodes the initial VMSA at this GPA and measures it during launch
/// finish, after all userspace-provided launch-update pages.
///
/// Keep the BSP context at this address until KVM supports userspace-supplied
/// VMSA pages. The MSHV path validates this address against the partition's
/// physical address width and maps separate userspace backing for the VMSA.
const KVM_VMSA_GPA: u64 = 0xffff_ffff_f000;
/// The low tables (SMBIOS, RSDP, and ACPI) stay below this address.
const LOW_TABLES_END: u64 = 1 << 20;
/// The kernel image and the measured payload start here.
const PAYLOAD_BASE: u64 = 1 << 20;
/// Extra boot-pool space for the EFI stub's other allocations: the boot
/// parameters, the command line, memory-map copies, the copied initrd if no
/// other accepted memory fits it, and alignment.
const BOOT_POOL_SLACK: u64 = 8 << 20;
/// The SMBIOS structure table follows the entry point, 16-byte aligned.
const SMBIOS_TABLE_OFFSET: u64 = (loader::smbios::ENTRY_POINT_SIZE as u64).next_multiple_of(16);
/// SMBIOS reports a fixed BIOS date. Linux accepts PCIe ECAM above 4 GiB only
/// when the BIOS year is 2010 or later (`acpi_mcfg_valid_entry` in
/// `arch/x86/pci/mmconfig-shared.c`).
const SMBIOS_BIOS_RELEASE_DATE: &str = "06/19/2026";

/// Inputs for the deterministic SNP Linux-direct guest layout.
pub struct BuildParams<'a> {
    /// The Linux payload configuration.
    pub linux: &'a LinuxImage,
    /// The number of 4-KiB RAM pages that the image measures. The guest can
    /// have more RAM, which the host device tree describes.
    pub memory_page_count: u64,
    /// The page-table address bit used as the SNP encryption bit.
    pub c_bit_position: u8,
    /// The SNP guest policy.
    pub policy: SnpPolicy,
    /// The SNP interrupt-injection mode.
    pub injection_type: &'a SnpInjectionType,
    /// The kernel, optional initrd, and bootshim resources.
    pub resources: &'a Resources,
}

/// The Linux payload, ready for placement.
struct Payload<'a> {
    /// The kernel's PE/COFF image.
    kernel: &'a PeImage,
    /// The bzImage `init_size`: the memory that the decompressed kernel needs.
    kernel_init_size: u64,
    initrd: Option<&'a [u8]>,
    command_line: &'a CStr,
}

fn read_linux_resources(
    linux: &LinuxImage,
    resources: &Resources,
) -> anyhow::Result<(Vec<u8>, Option<Vec<u8>>)> {
    let kernel_path = resources
        .get(ResourceType::LinuxKernel)
        .context("Linux kernel resource is missing")?;
    let kernel = fs_err::read(kernel_path)?;

    let initrd = if linux.use_initrd {
        let initrd_path = resources
            .get(ResourceType::LinuxInitrd)
            .context("Linux initrd resource is missing")?;
        Some(fs_err::read(initrd_path)?)
    } else {
        None
    };

    Ok((kernel, initrd))
}

fn new_loader(
    policy: SnpPolicy,
    c_bit_position: u8,
    memory_page_count: u64,
    injection_type: &SnpInjectionType,
) -> IgvmLoader<X86Register> {
    IgvmLoader::new_snp_linux_direct(SnpLinuxDirectConfig {
        policy,
        c_bit_mask: 1u64 << c_bit_position,
        ram_page_count: memory_page_count,
        vmsa_page: Some(KVM_VMSA_GPA / PAGE_SIZE),
        injection_type: match injection_type {
            SnpInjectionType::Normal => InjectionType::Normal,
            SnpInjectionType::Restricted => InjectionType::Restricted,
        },
    })
}

/// Builds the measured layout, Linux payload, and BSP launch context.
/// The shim creates ACPI from the host's unmeasured DeviceTree after launch.
pub fn build(params: BuildParams<'_>) -> anyhow::Result<IgvmOutput> {
    let BuildParams {
        linux,
        memory_page_count,
        c_bit_position,
        policy,
        injection_type,
        resources,
    } = params;

    let (kernel, initrd) = read_linux_resources(linux, resources)?;
    ensure!(
        loader::pe::is_pe(&kernel),
        "the SNP Linux kernel must be a bzImage with an EFI stub (CONFIG_EFI_STUB)"
    );
    let kernel_pe = loader::pe::layout(&kernel).context("parsing the kernel's PE/COFF image")?;
    let kernel_init_size = loader::bzimage::parse_bzimage(&mut std::io::Cursor::new(&kernel))
        .context("parsing the bzImage setup header")?
        .init_size
        .into();

    let bootshim_path = resources
        .get(ResourceType::SnpBootshim)
        .context("SNP bootshim resource is missing")?;
    let mut bootshim = fs_err::File::open(bootshim_path)?;

    let mut loader = new_loader(policy, c_bit_position, memory_page_count, injection_type);
    let bootshim_ranges = layout_guest(
        &mut loader,
        memory_page_count,
        c_bit_position,
        &Payload {
            kernel: &kernel_pe,
            kernel_init_size,
            initrd: initrd.as_deref(),
            command_line: &linux.command_line,
        },
        |loader, shim_base| {
            loader::elf::load_static_elf(
                &mut loader.loader(),
                &mut bootshim,
                0,
                shim_base,
                false,
                BootPageAcceptance::Exclusive,
                "snp-bootshim",
            )
            .context("loading SNP bootshim")
        },
    )?;

    let mut output = loader.finalize().context("finalizing SNP IGVM")?;
    for range in bootshim_ranges {
        output.map.report_range(
            MemoryRange::from_4k_gpn_range(range.start_gpn..range.start_gpn + range.page_count),
            "snp-bootshim-unaccepted-ram",
        );
    }
    Ok(output)
}

/// Places the whole measured guest image and returns the RAM ranges that it
/// does not import.
///
/// The layout is:
///
/// * Below 1 MiB: the GDT, the page tables, the SNP pages, the SMBIOS tables,
///   the RSDP page, and the ACPI output area.
/// * From 1 MiB: the kernel's PE/COFF image, the initrd, the UTF-16 command
///   line, the bootshim, its parameter and platform pages, the device-tree
///   parameter area, the ACPI heap, and the EFI stack.
/// * Then the boot pool, at or above 16 MiB, where the EFI stub places the
///   decompressed kernel.
///
/// The heap, the stack, and the boot pool are not imported. The shim accepts
/// them, and reports all other omitted RAM to Linux as unaccepted.
fn layout_guest(
    loader: &mut IgvmLoader<X86Register>,
    memory_page_count: u64,
    c_bit_position: u8,
    payload: &Payload<'_>,
    load_shim: impl FnOnce(&mut IgvmLoader<X86Register>, u64) -> anyhow::Result<loader::elf::LoadInfo>,
) -> anyhow::Result<Vec<SnpBootShimRange>> {
    let ram_end = memory_page_count
        .checked_mul(PAGE_SIZE)
        .context("RAM size overflow")?;
    let mut platform = SnpBootShimPlatformParams {
        magic: SNP_BOOT_SHIM_PLATFORM_MAGIC,
        version: SNP_BOOT_SHIM_PLATFORM_VERSION,
        c_bit_mask: 1u64 << c_bit_position,
        ..FromZeros::new_zeroed()
    };

    let (snp_pages, low_end) = loader::linux::import_snp_boot_stage(
        &mut loader.loader(),
        loader::linux::SnpBootConfig {
            c_bit: c_bit_position,
        },
    )
    .context("importing the SNP boot state")?;
    platform.secrets_gpa = snp_pages.secrets;
    platform.cpuid_gpa = snp_pages.cpuid;
    platform.cc_blob_gpa = snp_pages.cc_blob;

    let low_limit = LOW_TABLES_END.min(ram_end);
    let mut cursor = align_up(low_end, PAGE_SIZE)?;
    import_smbios(loader, &mut cursor, low_limit, &mut platform)?;
    platform.rsdp_gpa = reserve_region(&mut cursor, PAGE_SIZE, low_limit)?;
    platform.acpi_output_gpa = reserve_region(&mut cursor, SNP_BOOT_SHIM_ACPI_SIZE, low_limit)?;
    platform.acpi_output_size = SNP_BOOT_SHIM_ACPI_SIZE;
    {
        // Import zero-filled storage, not static ACPI. The shim publishes the
        // RSDP only after it validates and builds all tables.
        let mut importer = loader.loader();
        importer.import_pages(
            platform.rsdp_gpa / PAGE_SIZE,
            1,
            "snp-rsdp",
            BootPageAcceptance::Exclusive,
            &[],
        )?;
        importer.import_pages(
            platform.acpi_output_gpa / PAGE_SIZE,
            SNP_BOOT_SHIM_ACPI_SIZE / PAGE_SIZE,
            "snp-acpi-tables",
            BootPageAcceptance::Exclusive,
            &[],
        )?;
    }

    let mut cursor = align_up(PAYLOAD_BASE, payload.kernel.alignment.max(PAGE_SIZE))?;
    import_payload(loader, &mut cursor, ram_end, payload, &mut platform)?;

    let shim = load_shim(loader, align_up(cursor, PAGE_SIZE)?)?;
    ensure!(
        shim.minimum_address_used.is_multiple_of(PAGE_SIZE)
            && shim.minimum_address_used >= cursor
            && (shim.minimum_address_used..shim.next_available_address).contains(&shim.entrypoint),
        "invalid SNP bootshim range or entry point"
    );
    let mut cursor = align_up(shim.next_available_address, PAGE_SIZE)?;
    let params_gpa = reserve_region(&mut cursor, PAGE_SIZE, ram_end)?;
    let platform_gpa = reserve_region(&mut cursor, PAGE_SIZE, ram_end)?;
    platform.dt_gpa = reserve_region(&mut cursor, SNP_BOOT_SHIM_DT_SIZE, ram_end)?;
    platform.dt_size = SNP_BOOT_SHIM_DT_SIZE;
    platform.heap_gpa = reserve_region(&mut cursor, SNP_BOOT_SHIM_HEAP_SIZE, ram_end)?;
    platform.heap_size = SNP_BOOT_SHIM_HEAP_SIZE;
    platform.stack_gpa = reserve_region(&mut cursor, SNP_BOOT_SHIM_EFI_STACK_SIZE, ram_end)?;
    platform.stack_size = SNP_BOOT_SHIM_EFI_STACK_SIZE;

    let pool_size = align_up(payload.kernel_init_size, x86defs::X64_LARGE_PAGE_SIZE)?
        .checked_add(BOOT_POOL_SLACK)
        .context("boot pool size overflow")?;
    let mut pool_cursor = align_up(
        cursor.max(SNP_BOOT_SHIM_KERNEL_MIN_ADDRESS),
        x86defs::X64_LARGE_PAGE_SIZE,
    )?;
    platform.boot_pool_gpa = reserve_region(&mut pool_cursor, pool_size, ram_end)
        .context("the measured RAM is too small for the kernel's boot pool")?;
    platform.boot_pool_size = pool_size;

    import_handoff(
        loader,
        shim.entrypoint,
        params_gpa,
        platform_gpa,
        ram_end,
        &platform,
    )
}

/// Imports the SMBIOS entry point and structure table.
fn import_smbios(
    loader: &mut IgvmLoader<X86Register>,
    cursor: &mut u64,
    limit: u64,
    platform: &mut SnpBootShimPlatformParams,
) -> anyhow::Result<()> {
    let tables = loader::smbios::SmbiosTables {
        bios: loader::smbios::SmbiosBiosInfo {
            vendor: "OpenVMM",
            version: "OpenVMM SNP",
            release_date: SMBIOS_BIOS_RELEASE_DATE,
            major: 0,
            minor: 0,
        },
        system: loader::smbios::SmbiosSystemInfo {
            manufacturer: "OpenVMM",
            product_name: "OpenVMM Virtual Machine",
            version: "",
            serial_number: "",
            sku_number: "",
            family: "",
            uuid: [0; 16],
        },
    };
    let base = *cursor;
    let smbios = loader::smbios::build(&tables, base + SMBIOS_TABLE_OFFSET);
    let mut bytes = vec![0; SMBIOS_TABLE_OFFSET as usize];
    bytes[..smbios.entry_point.len()].copy_from_slice(&smbios.entry_point);
    bytes.extend_from_slice(&smbios.structure_table);
    let size = align_up(bytes.len() as u64, PAGE_SIZE)?;
    platform.smbios_gpa = reserve_region(cursor, size, limit)?;
    platform.smbios_size = size;
    loader.loader().import_pages(
        platform.smbios_gpa / PAGE_SIZE,
        size / PAGE_SIZE,
        "snp-smbios",
        BootPageAcceptance::Exclusive,
        &bytes,
    )?;
    Ok(())
}

/// Imports the kernel, the initrd, and the UTF-16 command line.
fn import_payload(
    loader: &mut IgvmLoader<X86Register>,
    cursor: &mut u64,
    ram_end: u64,
    payload: &Payload<'_>,
    platform: &mut SnpBootShimPlatformParams,
) -> anyhow::Result<()> {
    let kernel = payload.kernel;
    let kernel_size = kernel.image.len() as u64;
    platform.kernel_gpa = reserve_region(cursor, kernel_size, ram_end)
        .context("the measured RAM is too small for the kernel")?;
    platform.kernel_size = kernel_size;
    platform.kernel_entry = platform.kernel_gpa + kernel.entry_offset;
    loader.loader().import_pages(
        platform.kernel_gpa / PAGE_SIZE,
        kernel_size / PAGE_SIZE,
        "linux-kernel-pe",
        BootPageAcceptance::Exclusive,
        &kernel.image,
    )?;

    if let Some(initrd) = payload.initrd.filter(|initrd| !initrd.is_empty()) {
        let size = align_up(initrd.len() as u64, PAGE_SIZE)?;
        platform.initrd_gpa = reserve_region(cursor, size, ram_end)
            .context("the measured RAM is too small for the initrd")?;
        platform.initrd_size = initrd.len() as u64;
        loader.loader().import_pages(
            platform.initrd_gpa / PAGE_SIZE,
            size / PAGE_SIZE,
            "linux-initrd",
            BootPageAcceptance::Exclusive,
            initrd,
        )?;
    }

    let command_line = payload
        .command_line
        .to_str()
        .context("the kernel command line is not UTF-8")?
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<u8>>();
    let size = align_up(command_line.len() as u64, PAGE_SIZE)?;
    platform.cmdline_gpa = reserve_region(cursor, size, ram_end)?;
    platform.cmdline_size = command_line.len() as u64;
    loader.loader().import_pages(
        platform.cmdline_gpa / PAGE_SIZE,
        size / PAGE_SIZE,
        "linux-commandline",
        BootPageAcceptance::Exclusive,
        &command_line,
    )?;
    Ok(())
}

/// Imports the platform page, the device-tree parameter area, and the
/// parameter page, and starts the BSP at the shim.
///
/// Call this after every other import, so that the parameter page lists every
/// omitted RAM range.
fn import_handoff(
    loader: &mut IgvmLoader<X86Register>,
    shim_entry: u64,
    params_gpa: u64,
    platform_gpa: u64,
    ram_end: u64,
    platform: &SnpBootShimPlatformParams,
) -> anyhow::Result<Vec<SnpBootShimRange>> {
    {
        let mut importer = loader.loader();
        importer
            .import_pages(
                platform_gpa / PAGE_SIZE,
                1,
                "snp-bootshim-platform",
                BootPageAcceptance::Exclusive,
                platform.as_bytes(),
            )
            .context("importing SNP platform extension")?;
        // Host topology is unmeasured input. Full attestation policy is deferred.
        let area = importer
            .create_parameter_area(
                platform.dt_gpa / PAGE_SIZE,
                (platform.dt_size / PAGE_SIZE).try_into()?,
                "snp-bootshim-device-tree",
            )
            .context("reserving SNP device tree")?;
        importer
            .import_parameter(area, 0, IgvmParameterType::DeviceTree)
            .context("requesting SNP device tree")?;
    }

    let params_page = params_gpa / PAGE_SIZE;
    let bootshim_ranges = loader
        .unimported_ram_ranges([params_page])?
        .into_iter()
        .map(|range| SnpBootShimRange {
            start_gpn: range.start,
            page_count: range.end - range.start,
        })
        .collect::<Vec<_>>();

    let mut bootshim_params = build_bootshim_params(ram_end, &bootshim_ranges)?;
    bootshim_params.platform_gpa = platform_gpa;
    let mut importer = loader.loader();
    importer
        .import_pages(
            params_page,
            1,
            "snp-bootshim-params",
            BootPageAcceptance::Exclusive,
            bootshim_params.as_bytes(),
        )
        .context("importing SNP bootshim parameters")?;
    importer.import_vp_register(X86Register::Rip(shim_entry))?;
    importer.import_vp_register(X86Register::Rsi(params_gpa))?;

    Ok(bootshim_ranges)
}

fn align_up(value: u64, alignment: u64) -> anyhow::Result<u64> {
    value
        .checked_next_multiple_of(alignment)
        .context("alignment overflow")
}

fn reserve_region(cursor: &mut u64, size: u64, limit: u64) -> anyhow::Result<u64> {
    ensure!(
        cursor.is_multiple_of(PAGE_SIZE) && size.is_multiple_of(PAGE_SIZE),
        "unaligned SNP image region"
    );
    let start = *cursor;
    let end = start
        .checked_add(size)
        .context("SNP image region end overflow")?;
    ensure!(
        end <= limit,
        "SNP image region {start:#x}..{end:#x} lies above {limit:#x}"
    );
    *cursor = end;
    Ok(start)
}

fn build_bootshim_params(
    ram_end: u64,
    ranges: &[SnpBootShimRange],
) -> anyhow::Result<SnpBootShimParams> {
    ensure!(
        ranges.len() <= SNP_BOOT_SHIM_MAX_RANGES,
        "sparse SNP layout requires {} bootshim ranges, but the parameter page supports at most {}",
        ranges.len(),
        SNP_BOOT_SHIM_MAX_RANGES
    );
    let mut params = SnpBootShimParams::new_zeroed();
    params.magic = SNP_BOOT_SHIM_PARAMS_MAGIC;
    params.version = SNP_BOOT_SHIM_PARAMS_VERSION;
    params.range_count = ranges
        .len()
        .try_into()
        .context("SNP bootshim range count does not fit in u32")?;
    params.ram_end = ram_end;
    params.ranges[..ranges.len()].copy_from_slice(ranges);
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp_context_builder::VpContextBuilder;
    use crate::vp_context_builder::snp::SnpHardwareContext;
    use igvm::IgvmDirectiveHeader;
    use igvm::IgvmFile;
    use igvm::IgvmInitializationHeader;
    use igvm::IgvmPlatformHeader;
    use igvm::IgvmSerializer;
    use igvm_defs::IgvmPageDataType;
    use igvm_defs::IgvmPlatformType;
    use loader::importer::BootPageAcceptance;
    use loader::importer::ImageLoad;
    use std::collections::BTreeSet;
    use test_with_tracing::test;
    use zerocopy::FromBytes;

    const COMPATIBILITY_MASK: u32 = 1;
    const TEST_POLICY: u64 = 0x30000;
    const TEST_C_BIT_MASK: u64 = 1 << 51;
    const TEST_SHIM_ENTRY: u64 = 0x100000;

    fn test_loader_with_injection(
        ram_page_count: u64,
        injection_type: InjectionType,
    ) -> IgvmLoader<X86Register> {
        IgvmLoader::new_snp_linux_direct(SnpLinuxDirectConfig {
            policy: SnpPolicy::from(TEST_POLICY),
            c_bit_mask: TEST_C_BIT_MASK,
            ram_page_count,
            vmsa_page: Some(KVM_VMSA_GPA / PAGE_SIZE),
            injection_type,
        })
    }

    fn test_loader(ram_page_count: u64) -> IgvmLoader<X86Register> {
        test_loader_with_injection(ram_page_count, InjectionType::Normal)
    }

    fn import_test_registers(importer: &mut dyn ImageLoad<X86Register>, params_gpa: u64) {
        for register in [
            X86Register::Rip(TEST_SHIM_ENTRY),
            X86Register::Rsi(params_gpa),
            X86Register::Cr3(0x4000 | TEST_C_BIT_MASK),
            X86Register::Cr0(x86defs::X64_CR0_PE | x86defs::X64_CR0_PG),
            X86Register::Cr4(x86defs::X64_CR4_PAE),
            X86Register::Efer(x86defs::X64_EFER_LME | x86defs::X64_EFER_LMA),
        ] {
            importer.import_vp_register(register).unwrap();
        }
    }

    fn assert_serialized_igvm_shape(
        igvm: &IgvmFile,
        ram_page_count: u64,
        expected_pages: &[u64],
        expected_handoff: (u64, u64),
    ) {
        let serializer = IgvmSerializer::new(igvm).unwrap();
        let expected_launch_digest = serializer
            .measurement_for(IgvmPlatformType::SEV_SNP)
            .unwrap()
            .digest
            .clone();
        let mut binary = Vec::new();
        serializer.serialize(&mut binary).unwrap();
        let reparsed = IgvmFile::new_from_binary(&binary, Some(igvm::IsolationType::Snp)).unwrap();

        assert_eq!(reparsed.platforms().len(), 1);
        let IgvmPlatformHeader::SupportedPlatform(platform) = &reparsed.platforms()[0];
        assert_eq!(platform.platform_type, IgvmPlatformType::SEV_SNP);
        assert_eq!(platform.highest_vtl, 0);
        assert_eq!(platform.shared_gpa_boundary, 0);
        assert!(reparsed.initializations().iter().any(|header| matches!(
            header,
            IgvmInitializationHeader::GuestPolicy {
                policy: TEST_POLICY,
                compatibility_mask: COMPATIBILITY_MASK,
            }
        )));

        let mut covered_pages = BTreeSet::new();
        let mut required_memory_count = 0;
        let mut next_vmsa_index = 0;
        let mut secrets_count = 0;
        let mut cpuid_count = 0;

        for directive in reparsed.directives() {
            match directive {
                IgvmDirectiveHeader::PageData {
                    gpa,
                    flags,
                    data_type,
                    data,
                    ..
                } => {
                    assert!(gpa.is_multiple_of(PAGE_SIZE));
                    let page = gpa / PAGE_SIZE;
                    assert!(page < ram_page_count);
                    assert!(covered_pages.insert(page));
                    assert!(!flags.shared() && !flags.unmeasured());
                    assert!(data.len() <= PAGE_SIZE as usize);
                    match *data_type {
                        IgvmPageDataType::SECRETS => secrets_count += 1,
                        IgvmPageDataType::CPUID_DATA => cpuid_count += 1,
                        IgvmPageDataType::NORMAL => {}
                        unexpected => panic!("unexpected PageData type {unexpected:?}"),
                    }
                }
                IgvmDirectiveHeader::SnpVpContext {
                    gpa,
                    vp_index,
                    vmsa,
                    ..
                } => {
                    assert_eq!(*gpa, KVM_VMSA_GPA);
                    assert_eq!(u32::from(*vp_index), next_vmsa_index);
                    assert_eq!(vmsa.rip, expected_handoff.0);
                    assert_eq!(vmsa.rsi, expected_handoff.1);
                    assert!(vmsa.sev_features.snp());
                    assert!(!vmsa.sev_features.vtom());
                    assert_eq!(vmsa.virtual_tom, 0);
                    assert_ne!(vmsa.cr3 & TEST_C_BIT_MASK, 0);
                    assert_ne!(vmsa.cr0 & x86defs::X64_CR0_ET, 0);
                    assert_ne!(vmsa.cr4 & x86defs::X64_CR4_MCE, 0);
                    assert_eq!(vmsa.rflags, u64::from(x86defs::RFlags::at_reset()));
                    assert_eq!(vmsa.dr6, 0xffff_0ff0);
                    assert_eq!(vmsa.dr7, 0x400);
                    assert_eq!(vmsa.x87_fcw, x86defs::xsave::INIT_FCW);
                    assert_eq!(vmsa.mxcsr, x86defs::xsave::DEFAULT_MXCSR);
                    next_vmsa_index += 1;
                }
                IgvmDirectiveHeader::RequiredMemory {
                    gpa,
                    number_of_bytes,
                    vtl2_protectable,
                    ..
                } => {
                    assert_eq!(*gpa, 0);
                    assert_eq!(u64::from(*number_of_bytes), ram_page_count * PAGE_SIZE);
                    assert!(!vtl2_protectable);
                    required_memory_count += 1;
                }
                unexpected => panic!("unexpected directive in fixed SNP IGVM: {unexpected:?}"),
            }
        }

        assert_eq!(
            covered_pages,
            expected_pages.iter().copied().collect::<BTreeSet<_>>()
        );
        assert_eq!(required_memory_count, 1);
        assert_eq!(next_vmsa_index, 1);
        assert_eq!(secrets_count, 1);
        assert_eq!(cpuid_count, 1);

        let measured_digest = IgvmSerializer::new(&reparsed)
            .unwrap()
            .measurement_for(IgvmPlatformType::SEV_SNP)
            .unwrap()
            .digest
            .clone();
        assert_eq!(measured_digest, expected_launch_digest);
    }

    #[test]
    fn vmsa_uses_c_bit_model() {
        let params_gpa = 0x6000;
        let mut context =
            SnpHardwareContext::new_linux_direct(TEST_C_BIT_MASK, InjectionType::Normal);
        for register in [
            X86Register::Rip(TEST_SHIM_ENTRY),
            X86Register::Rsi(params_gpa),
            X86Register::Cr3(0x4000 | TEST_C_BIT_MASK),
            X86Register::Cr0(x86defs::X64_CR0_PE | x86defs::X64_CR0_PG),
            X86Register::Cr4(x86defs::X64_CR4_PAE),
            X86Register::Efer(x86defs::X64_EFER_LME | x86defs::X64_EFER_LMA),
        ] {
            context.import_vp_register(register);
        }

        let vmsa = context.vmsa();
        assert_eq!(vmsa.rip, TEST_SHIM_ENTRY);
        assert_eq!(vmsa.rsi, params_gpa);
        assert_ne!(vmsa.cr3 & TEST_C_BIT_MASK, 0);
        assert_ne!(vmsa.cr0 & x86defs::X64_CR0_ET, 0);
        assert_ne!(vmsa.cr4 & x86defs::X64_CR4_MCE, 0);
        assert!(vmsa.sev_features.snp());
        assert!(!vmsa.sev_features.vtom());
        assert!(!vmsa.sev_features.debug_swap());
        assert_eq!(vmsa.virtual_tom, 0);
        assert_eq!(vmsa.rflags, u64::from(x86defs::RFlags::at_reset()));
        assert_eq!(vmsa.dr6, 0xffff_0ff0);
        assert_eq!(vmsa.dr7, 0x400);
        assert_eq!(vmsa.tr.limit, 0xffff);
        assert_eq!(vmsa.tr.attrib, 0x8b);
        assert_eq!(vmsa.ldtr.limit, 0xffff);
        assert_eq!(vmsa.ldtr.attrib, 0x82);
        assert_eq!(vmsa.idtr.limit, 0xffff);
        assert_eq!(vmsa.x87_fcw, x86defs::xsave::INIT_FCW);
        assert_eq!(vmsa.mxcsr, x86defs::xsave::DEFAULT_MXCSR);
    }

    #[test]
    fn normal_and_restricted_vmsas_use_the_selected_injection_mode() {
        for (injection_type, restricted) in [
            (InjectionType::Normal, false),
            (InjectionType::Restricted, true),
        ] {
            let mut loader = test_loader_with_injection(1, injection_type);
            import_test_registers(&mut loader.loader(), 0x6000);
            let output = loader.finalize().unwrap();
            let vmsa = output
                .guest
                .directives()
                .iter()
                .find_map(|directive| match directive {
                    IgvmDirectiveHeader::SnpVpContext { vmsa, .. } => Some(vmsa),
                    _ => None,
                })
                .unwrap();
            assert!(vmsa.sev_features.snp());
            assert_eq!(vmsa.sev_features.restrict_injection(), restricted);
            assert!(!vmsa.sev_features.alternate_injection());
        }
    }

    #[test]
    fn sparse_igvm_serializes_and_preserves_measurement() {
        const RAM_PAGE_COUNT: u64 = 8;
        const PARAMS_PAGE: u64 = 6;
        let params_gpa = PARAMS_PAGE * PAGE_SIZE;
        let mut loader = test_loader(RAM_PAGE_COUNT);
        {
            let mut importer = loader.loader();
            importer
                .import_pages(1, 1, "secrets", BootPageAcceptance::SecretsPage, &[])
                .unwrap();
            importer
                .import_pages(2, 1, "cpuid", BootPageAcceptance::CpuidPage, &[])
                .unwrap();
        }

        let ranges = loader.unimported_ram_ranges([PARAMS_PAGE]).unwrap();
        assert_eq!(ranges, [0..1, 3..6, 7..8]);
        let ranges = ranges
            .iter()
            .map(|range| SnpBootShimRange {
                start_gpn: range.start,
                page_count: range.end - range.start,
            })
            .collect::<Vec<_>>();
        let params = build_bootshim_params(RAM_PAGE_COUNT * PAGE_SIZE, &ranges).unwrap();
        {
            let mut importer = loader.loader();
            importer
                .import_pages(
                    PARAMS_PAGE,
                    1,
                    "snp-bootshim-params",
                    BootPageAcceptance::Exclusive,
                    params.as_bytes(),
                )
                .unwrap();
            import_test_registers(&mut importer, params_gpa);
        }

        let output = loader.finalize().unwrap();
        let mut imported_pages = Vec::new();
        let mut required_memory = None;
        let mut handoff = None;
        let mut serialized_params = None;
        for directive in output.guest.directives() {
            match directive {
                IgvmDirectiveHeader::PageData {
                    gpa,
                    data_type,
                    data,
                    ..
                } => {
                    imported_pages.push(*gpa / PAGE_SIZE);
                    if *gpa == params_gpa {
                        assert_eq!(*data_type, IgvmPageDataType::NORMAL);
                        let mut page = [0; PAGE_SIZE as usize];
                        page[..data.len()].copy_from_slice(data);
                        serialized_params =
                            Some(SnpBootShimParams::read_from_bytes(&page).unwrap());
                    }
                }
                IgvmDirectiveHeader::RequiredMemory {
                    gpa,
                    number_of_bytes,
                    ..
                } => required_memory = Some((*gpa, *number_of_bytes)),
                IgvmDirectiveHeader::SnpVpContext { vmsa, .. } => {
                    handoff = Some((vmsa.rip, vmsa.rsi));
                }
                _ => {}
            }
        }

        imported_pages.sort_unstable();
        assert_eq!(imported_pages, [1, 2, PARAMS_PAGE]);
        assert_eq!(
            required_memory,
            Some((0, (RAM_PAGE_COUNT * PAGE_SIZE) as u32))
        );
        assert_eq!(handoff, Some((TEST_SHIM_ENTRY, params_gpa)));
        assert_eq!(serialized_params, Some(params));
        assert_serialized_igvm_shape(
            &output.guest,
            RAM_PAGE_COUNT,
            &[1, 2, PARAMS_PAGE],
            (TEST_SHIM_ENTRY, params_gpa),
        );
    }

    #[test]
    fn sparse_image_emits_only_the_bsp_vmsa() {
        let mut loader = test_loader(1);
        import_test_registers(&mut loader.loader(), 0x2000);

        let output = loader.finalize().unwrap();
        let vp_indexes = output
            .guest
            .directives()
            .iter()
            .filter_map(|directive| match directive {
                IgvmDirectiveHeader::SnpVpContext { vp_index, .. } => Some(*vp_index),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(vp_indexes, [0]);
    }

    #[test]
    fn overlapping_import_does_not_mutate_existing_pages() {
        let mut loader = test_loader(4);
        {
            let mut importer = loader.loader();
            importer
                .import_pages(1, 1, "first", BootPageAcceptance::Exclusive, &[0xaa])
                .unwrap();
            let error = importer
                .import_pages(
                    0,
                    2,
                    "overlap",
                    BootPageAcceptance::Exclusive,
                    &[0xbb; PAGE_SIZE as usize * 2],
                )
                .unwrap_err();
            assert!(error.to_string().contains("overlaps"));
            import_test_registers(&mut importer, 0x2000);
        }

        let output = loader.finalize().unwrap();
        let page = output
            .guest
            .directives()
            .iter()
            .find_map(|directive| match directive {
                IgvmDirectiveHeader::PageData { gpa, data, .. } if *gpa == PAGE_SIZE => Some(data),
                _ => None,
            })
            .unwrap();
        assert_eq!(page, &[0xaa]);
    }

    #[test]
    fn rejects_too_many_bootshim_ranges() {
        let ranges = vec![
            SnpBootShimRange {
                start_gpn: 0,
                page_count: 1,
            };
            SNP_BOOT_SHIM_MAX_RANGES + 1
        ];
        assert!(
            build_bootshim_params(0x200000, &ranges)
                .unwrap_err()
                .to_string()
                .contains("supports at most")
        );
    }

    const LAYOUT_RAM_PAGES: u64 = 40960;
    const SHIM_PAGES: u64 = 2;
    const KERNEL_INIT_SIZE: u64 = 0x2d2_1000;

    struct Layout {
        output: IgvmOutput,
        ranges: Vec<SnpBootShimRange>,
        shim_gpa: u64,
        kernel: PeImage,
        initrd: Vec<u8>,
    }

    fn test_kernel() -> PeImage {
        let mut image = vec![0; 0x5000];
        image[..2].copy_from_slice(b"MZ");
        image[0x4000..].fill(0xcc);
        PeImage {
            image,
            entry_offset: 0x4010,
            alignment: PAGE_SIZE,
        }
    }

    fn try_layout(ram_pages: u64, initrd: Option<&[u8]>) -> anyhow::Result<Layout> {
        let kernel = test_kernel();
        let mut loader = test_loader(ram_pages);
        let mut shim_gpa = 0;
        let ranges = layout_guest(
            &mut loader,
            ram_pages,
            51,
            &Payload {
                kernel: &kernel,
                kernel_init_size: KERNEL_INIT_SIZE,
                initrd,
                command_line: c"console=ttyS0",
            },
            |loader, gpa| {
                shim_gpa = gpa;
                loader.loader().import_pages(
                    gpa / PAGE_SIZE,
                    SHIM_PAGES,
                    "test-shim",
                    BootPageAcceptance::Exclusive,
                    &[],
                )?;
                Ok(loader::elf::LoadInfo {
                    minimum_address_used: gpa,
                    next_available_address: gpa + SHIM_PAGES * PAGE_SIZE,
                    entrypoint: gpa,
                })
            },
        )?;
        Ok(Layout {
            output: loader.finalize()?,
            ranges,
            shim_gpa,
            kernel,
            initrd: initrd.unwrap_or_default().to_vec(),
        })
    }

    fn generated_layout() -> Layout {
        try_layout(LAYOUT_RAM_PAGES, Some(&[0x5a; 0x1800])).unwrap()
    }

    fn imported_bytes(output: &IgvmOutput, address: u64, len: u64) -> Vec<u8> {
        (address..address + len)
            .step_by(PAGE_SIZE as usize)
            .flat_map(|gpa| imported_page(output, gpa))
            .take(len as usize)
            .collect()
    }

    fn imported_page(output: &IgvmOutput, address: u64) -> [u8; PAGE_SIZE as usize] {
        let data = output
            .guest
            .directives()
            .iter()
            .find_map(|directive| match directive {
                IgvmDirectiveHeader::PageData { gpa, data, .. } if *gpa == address => Some(data),
                _ => None,
            })
            .unwrap();
        let mut page = [0; PAGE_SIZE as usize];
        page[..data.len()].copy_from_slice(data);
        page
    }

    fn handoff(layout: &Layout) -> (SnpBootShimParams, SnpBootShimPlatformParams) {
        let params_gpa = layout.shim_gpa + SHIM_PAGES * PAGE_SIZE;
        let params =
            SnpBootShimParams::read_from_bytes(&imported_page(&layout.output, params_gpa)).unwrap();
        let (platform, _) = SnpBootShimPlatformParams::read_from_prefix(&imported_page(
            &layout.output,
            params.platform_gpa,
        ))
        .unwrap();
        (params, platform)
    }

    fn contains(ranges: &[SnpBootShimRange], gpa: u64, size: u64) -> bool {
        ranges.iter().any(|range| {
            let start = range.start_gpn * PAGE_SIZE;
            start <= gpa && gpa + size <= start + range.page_count * PAGE_SIZE
        })
    }

    #[test]
    fn layout_covers_every_page_once_and_leaves_work_areas_unimported() {
        let layout = generated_layout();
        let (params, platform) = handoff(&layout);
        assert_eq!(params.version, SNP_BOOT_SHIM_PARAMS_VERSION);
        assert_eq!(params.ram_end, LAYOUT_RAM_PAGES * PAGE_SIZE);
        assert_eq!(&params.ranges[..params.range_count as usize], layout.ranges);
        assert_eq!(platform.magic, SNP_BOOT_SHIM_PLATFORM_MAGIC);
        assert_eq!(platform.version, SNP_BOOT_SHIM_PLATFORM_VERSION);
        assert_eq!(platform.c_bit_mask, TEST_C_BIT_MASK);

        let mut imported = BTreeSet::new();
        let mut dt_count = 0;
        for directive in layout.output.guest.directives() {
            match directive {
                IgvmDirectiveHeader::PageData { gpa, .. } => {
                    assert!(imported.insert(gpa / PAGE_SIZE));
                }
                IgvmDirectiveHeader::ParameterInsert(info) => {
                    assert_eq!(info.gpa, platform.dt_gpa);
                    dt_count += 1;
                }
                _ => {}
            }
        }
        assert_eq!(dt_count, 1);
        let dt_pages =
            platform.dt_gpa / PAGE_SIZE..(platform.dt_gpa + platform.dt_size) / PAGE_SIZE;
        for page in 0..LAYOUT_RAM_PAGES {
            let unimported = usize::from(contains(&layout.ranges, page * PAGE_SIZE, PAGE_SIZE));
            assert_eq!(
                unimported
                    + usize::from(imported.contains(&page))
                    + usize::from(dt_pages.contains(&page)),
                1,
                "page {page:#x} must be covered exactly once"
            );
        }

        for (gpa, size) in [
            (platform.heap_gpa, platform.heap_size),
            (platform.stack_gpa, platform.stack_size),
            (platform.boot_pool_gpa, platform.boot_pool_size),
        ] {
            assert_ne!(size, 0);
            assert!(
                contains(&layout.ranges, gpa, size),
                "{gpa:#x} must be unimported"
            );
        }
        assert_eq!(platform.stack_size, SNP_BOOT_SHIM_EFI_STACK_SIZE);
    }

    #[test]
    fn boot_pool_fits_the_decompressed_kernel() {
        let layout = generated_layout();
        let (_, platform) = handoff(&layout);
        assert!(platform.boot_pool_gpa >= SNP_BOOT_SHIM_KERNEL_MIN_ADDRESS);
        assert!(
            platform
                .boot_pool_gpa
                .is_multiple_of(x86defs::X64_LARGE_PAGE_SIZE)
        );
        assert!(platform.boot_pool_size >= KERNEL_INIT_SIZE + BOOT_POOL_SLACK);
        assert!(platform.boot_pool_gpa + platform.boot_pool_size <= LAYOUT_RAM_PAGES * PAGE_SIZE);
        // The pool follows every import.
        assert!(platform.stack_gpa + platform.stack_size <= platform.boot_pool_gpa);
    }

    #[test]
    fn payload_is_imported_as_described() {
        let layout = generated_layout();
        let (_, platform) = handoff(&layout);
        assert_eq!(platform.kernel_gpa, PAYLOAD_BASE);
        assert_eq!(platform.kernel_size, layout.kernel.image.len() as u64);
        assert_eq!(
            platform.kernel_entry,
            PAYLOAD_BASE + layout.kernel.entry_offset
        );
        assert_eq!(
            imported_bytes(&layout.output, platform.kernel_gpa, platform.kernel_size),
            layout.kernel.image
        );
        assert_eq!(platform.initrd_size, layout.initrd.len() as u64);
        assert_eq!(
            imported_bytes(&layout.output, platform.initrd_gpa, platform.initrd_size),
            layout.initrd
        );
        let command_line = "console=ttyS0\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(platform.cmdline_size, command_line.len() as u64);
        assert_eq!(
            imported_bytes(&layout.output, platform.cmdline_gpa, platform.cmdline_size),
            command_line
        );
    }

    #[test]
    fn layout_without_initrd_reports_none() {
        let layout = try_layout(LAYOUT_RAM_PAGES, None).unwrap();
        let (_, platform) = handoff(&layout);
        assert_eq!((platform.initrd_gpa, platform.initrd_size), (0, 0));
    }

    #[test]
    fn low_tables_and_snp_pages_are_below_1mib() {
        let layout = generated_layout();
        let (_, platform) = handoff(&layout);
        for (gpa, size) in [
            (platform.smbios_gpa, platform.smbios_size),
            (platform.rsdp_gpa, PAGE_SIZE),
            (platform.acpi_output_gpa, platform.acpi_output_size),
            (platform.secrets_gpa, PAGE_SIZE),
            (platform.cpuid_gpa, PAGE_SIZE),
            (platform.cc_blob_gpa, PAGE_SIZE),
        ] {
            assert!(gpa + size <= LOW_TABLES_END);
        }
        assert_eq!(platform.acpi_output_size, SNP_BOOT_SHIM_ACPI_SIZE);
        // ACPI output starts empty. The shim builds it.
        for gpa in (platform.rsdp_gpa..platform.acpi_output_gpa + platform.acpi_output_size)
            .step_by(PAGE_SIZE as usize)
        {
            assert_eq!(imported_page(&layout.output, gpa), [0; PAGE_SIZE as usize]);
        }
        let secrets = layout
            .output
            .guest
            .directives()
            .iter()
            .find_map(|directive| match directive {
                IgvmDirectiveHeader::PageData { gpa, data_type, .. }
                    if *data_type == IgvmPageDataType::SECRETS =>
                {
                    Some(*gpa)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(secrets, platform.secrets_gpa);
    }

    #[test]
    fn smbios_reports_a_recent_bios_date() {
        let layout = generated_layout();
        let (_, platform) = handoff(&layout);
        let smbios = imported_bytes(&layout.output, platform.smbios_gpa, platform.smbios_size);
        assert_eq!(&smbios[..5], b"_SM3_");
        let table_address = u64::from_le_bytes(smbios[0x10..0x18].try_into().unwrap());
        assert_eq!(table_address, platform.smbios_gpa + SMBIOS_TABLE_OFFSET);
        let date = SMBIOS_BIOS_RELEASE_DATE.as_bytes();
        assert!(smbios.windows(date.len()).any(|window| window == date));
        let year: u32 = SMBIOS_BIOS_RELEASE_DATE[6..].parse().unwrap();
        assert!(year >= 2010);
    }

    #[test]
    fn rejects_ram_too_small_for_the_boot_pool() {
        let error = try_layout(0x4000, Some(&[1; 16])).err().unwrap();
        assert!(format!("{error:#}").contains("boot pool"), "{error:#}");
    }

    #[test]
    fn rejects_region_overflow() {
        let mut cursor = !(PAGE_SIZE - 1);
        assert!(reserve_region(&mut cursor, PAGE_SIZE, u64::MAX).is_err());
        assert!(align_up(u64::MAX, PAGE_SIZE).is_err());
    }
}
