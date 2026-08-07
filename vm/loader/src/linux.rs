// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux specific loader definitions and implementation.

use crate::common::ChunkBuf;
use crate::common::ImportFileRegion;
use crate::common::ImportFileRegionError;
use crate::common::ReadSeek;
use crate::common::import_default_gdt;
use crate::elf::load_static_elf;
use crate::importer::Aarch64Register;
use crate::importer::BootPageAcceptance;
use crate::importer::GuestArch;
use crate::importer::ImageLoad;
use crate::importer::X86Register;
use aarch64defs::Cpsr64;
use aarch64defs::IntermPhysAddrSize;
use aarch64defs::SctlrEl1;
use aarch64defs::TranslationBaseEl1;
use aarch64defs::TranslationControlEl1;
use aarch64defs::TranslationGranule0;
use aarch64defs::TranslationGranule1;
use bitfield_struct::bitfield;
use hvdef::HV_PAGE_SIZE;
use loader_defs::linux as defs;
use memory_range::MemoryRange;
use page_table::IdentityMapSize;
use page_table::x64::IdentityMapBuilder;
use page_table::x64::PAGE_TABLE_MAX_BYTES;
use page_table::x64::PAGE_TABLE_MAX_COUNT;
use page_table::x64::PageTable;
use page_table::x64::align_up_to_large_page_size;
use page_table::x64::align_up_to_page_size;
use std::ffi::CString;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::mem::size_of;
use thiserror::Error;
use vm_topology::memory::MemoryLayout;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

struct ZeroPageBuildResult {
    boot_params: defs::boot_params,
    additional_pages: Option<MemoryRange>,
}

/// Construct a zero page from the following parameters.
fn build_zero_page(
    mem_layout: &MemoryLayout,
    acpi_len: usize,
    smbios_struct_len: usize,
    additional_page_count: u64,
    cmdline: &CString,
    initrd_base: u32,
    initrd_size: u32,
    bzimage_header: Option<&defs::setup_header>,
) -> Result<ZeroPageBuildResult, Error> {
    // Loader type 0xff = unregistered bootloader, used for both ELF and
    // bzImage paths since OpenVMM does not have a registered Linux
    // bootloader ID.
    const LOADER_TYPE_UNREGISTERED: u8 = 0xff;

    // Start with the bzImage setup header if available, otherwise build
    // a minimal default header.
    let mut hdr = match bzimage_header {
        Some(orig) => *orig,
        None => defs::setup_header {
            boot_flag: 0xaa55.into(),
            header: 0x53726448.into(),
            kernel_alignment: 0x100000.into(),
            ..FromZeros::new_zeroed()
        },
    };

    // Set bootloader-owned fields regardless of kernel format.
    hdr.type_of_loader = LOADER_TYPE_UNREGISTERED;
    hdr.cmd_line_ptr = CMDLINE_BASE.try_into().expect("must fit in u32");
    hdr.cmdline_size = (cmdline.as_bytes().len() as u64)
        .try_into()
        .expect("must fit in u32");
    hdr.ramdisk_image = initrd_base.into();
    hdr.ramdisk_size = initrd_size.into();

    let mut p = defs::boot_params {
        hdr,
        ..FromZeros::new_zeroed()
    };

    let mut ram = mem_layout.ram().iter().cloned();
    let range = ram.next().expect("at least one ram range");
    assert_eq!(range.range.start(), 0);
    assert!(range.range.end() >= 0x100000);

    // x86 low-memory layout for direct boot:
    //   [0, acpi_base)          RAM       boot metadata: GDT, zero page, cmdline,
    //                                     identity-map page tables
    //   [acpi_base, acpi_end)   ACPI      RSDT/XSDT and all ACPI tables
    //   [acpi_end, smbios_end)  RESERVED  SMBIOS structure table
    //   [smbios_end, additional_end) RESERVED additional requested pages
    //   [additional_end, 0xe0000)    RAM
    //   [0xe0000, 0x100000)     RESERVED  legacy BIOS region holding the RSDP at
    //                                     0xe0000 (found by the kernel's legacy
    //                                     scan) and the SMBIOS _SM3_ anchor at
    //                                     0xf0000 (found by the kernel's DMI scan)
    //   [0x100000, end)         RAM
    //
    // The RSDP lives at the fixed 0xe0000 and the tables it points to live in
    // reclaimable ACPI memory below; the kernel discovers the RSDP via its
    // legacy scan, so no `acpi_rsdp_addr` (Linux 5.0+) is required. The SMBIOS
    // structure table sits just above the ACPI tables in its own reserved
    // region so it can grow well past the 64 KiB F-segment.
    const ONE_MB: u64 = 0x100000;
    let aligned_acpi_len = align_up_to_page_size(acpi_len as u64);
    let acpi_end = ACPI_TABLES_BASE + aligned_acpi_len;
    let aligned_smbios_len = align_up_to_page_size(smbios_struct_len as u64);
    let smbios_end = acpi_end + aligned_smbios_len;
    let additional_size = additional_page_count
        .checked_mul(HV_PAGE_SIZE)
        .ok_or(Error::LowTablesTooLarge(u64::MAX, RSDP_BASE))?;
    let additional_end = smbios_end
        .checked_add(additional_size)
        .ok_or(Error::LowTablesTooLarge(u64::MAX, RSDP_BASE))?;
    if additional_end > RSDP_BASE {
        return Err(Error::LowTablesTooLarge(additional_end, RSDP_BASE));
    }
    let additional_pages =
        (additional_size != 0).then(|| MemoryRange::new(smbios_end..additional_end));

    // Emit the e820 entries in ascending address order. Zero-length regions
    // (e.g. the SMBIOS reserved region when no SMBIOS tables are present) are
    // skipped, and the fixed-size map is bounds-checked so an over-long memory
    // layout is reported rather than panicking.
    let e820_cap = p.e820_map.len();
    let mut n = 0;
    let mut push = |addr: u64, size: u64, typ: u32| -> Result<(), Error> {
        if size == 0 {
            return Ok(());
        }
        let entry = p
            .e820_map
            .get_mut(n)
            .ok_or(Error::TooManyMemoryRanges(e820_cap))?;
        *entry = defs::e820entry {
            addr: addr.into(),
            size: size.into(),
            typ: typ.into(),
        };
        n += 1;
        Ok(())
    };
    push(0, ACPI_TABLES_BASE, defs::E820_RAM)?;
    push(ACPI_TABLES_BASE, aligned_acpi_len, defs::E820_ACPI)?;
    push(acpi_end, aligned_smbios_len, defs::E820_RESERVED)?;
    push(smbios_end, additional_size, defs::E820_RESERVED)?;
    push(additional_end, RSDP_BASE - additional_end, defs::E820_RAM)?;
    push(RSDP_BASE, ONE_MB - RSDP_BASE, defs::E820_RESERVED)?;
    push(ONE_MB, range.range.end() - ONE_MB, defs::E820_RAM)?;
    for range in ram {
        push(range.range.start(), range.range.len(), defs::E820_RAM)?;
    }
    p.e820_entries = n as u8;

    Ok(ZeroPageBuildResult {
        boot_params: p,
        additional_pages,
    })
}

#[derive(Debug, Error)]
pub enum FlatLoaderError {
    #[error("unsupported ELF File byte order")]
    BigEndianElfOnLittle,
    #[error("error reading kernel data structure")]
    BadImageMagic,
    #[error("big-endian kernel image is not supported")]
    BigEndianKernelImage,
    #[error("only images with 4K pages are supported")]
    FourKibPageImageIsRequired,
    #[error("the kernel is required to run in the low memory; not supported")]
    LowMemoryKernel,
    #[error("failed to read kernel image")]
    ReadKernelImage,
    #[error("failed to seek to file offset as pointed by the ELF program header")]
    SeekKernelStart,
    #[error("failed to seek to offset of kernel image")]
    SeekKernelImage,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("elf loader error")]
    ElfLoader(#[source] crate::elf::Error),
    #[error("bzImage parse error")]
    BzImage(#[source] crate::bzimage::Error),
    #[error("flat loader error")]
    FlatLoader(#[source] FlatLoaderError),
    #[error("Address is not page aligned")]
    UnalignedAddress(u64),
    #[error("importer error")]
    Importer(#[source] anyhow::Error),
    #[error("failed to import initrd")]
    ImportInitrd(#[source] ImportFileRegionError),
    #[error("failed to import bzImage payload")]
    ImportBzImage(#[source] ImportFileRegionError),
    #[error("PageTableBuilder: {0}")]
    PageTableBuilder(#[from] page_table::Error),
    #[error("kernel command line ({0} bytes) exceeds its {1:#x}-byte slot")]
    CommandLineTooLong(usize, u64),
    #[error("low-memory tables and boot pages end at {0:#x}, past the reserved region at {1:#x}")]
    LowTablesTooLarge(u64, u64),
    #[error("SNP CC blob address {0:#x} does not fit in the Linux boot protocol field")]
    SnpCcBlobAddressTooHigh(u64),
    #[error("too many memory ranges to fit in the {0}-entry e820 map")]
    TooManyMemoryRanges(usize),
    #[error("acpi tables are empty")]
    EmptyAcpiTables,
}

/// ACPI tables to place in guest memory: a one-page RSDP plus the tables it
/// points to.
///
/// Produced by the caller-supplied builder passed to [`load_x86`] /
/// [`load_config_x86`]. The builder is handed a nominal RSDP address `gpa` and
/// must return `tables` that are self-consistent for placement at `gpa +
/// 0x1000`; the loader then re-homes the RSDP to the fixed legacy-scan address.
pub struct AcpiTables {
    /// The RSDP. Given a whole page.
    pub rsdp: Vec<u8>,
    /// The remaining tables pointed to by the RSDP.
    pub tables: Vec<u8>,
}

// The loader owns the entire sub-1 MB x86 direct-boot memory map so that
// callers supply only table *contents*, never addresses. The resulting e820
// map (see `build_zero_page`) is:
//
//   [0, ACPI_TABLES_BASE)          RAM       GDT, zero page, cmdline, page tables
//   [ACPI_TABLES_BASE, acpi_end)   ACPI      RSDT/XSDT and all ACPI tables
//   [acpi_end, smbios_end)         RESERVED  SMBIOS structure table
//   [smbios_end, RSDP_BASE)        RAM
//   [RSDP_BASE, 0x100000)          RESERVED  RSDP (0xe0000) + _SM3_ anchor (0xf0000)
//   [0x100000, end)                RAM       kernel and beyond
const GDT_BASE: u64 = 0x1000;
const ZERO_PAGE_BASE: u64 = 0x2000;
const CMDLINE_BASE: u64 = 0x3000;
const CR3_BASE: u64 = 0x4000;
/// The identity-map page tables occupy `[CR3_BASE, CR3_BASE + PAGE_TABLE_MAX_BYTES)`;
/// the boot metadata ends there.
const LOW_METADATA_END: u64 = CR3_BASE + PAGE_TABLE_MAX_BYTES as u64;
/// The ACPI builder is handed `LOW_METADATA_END` as a nominal RSDP page, so its
/// tables live one page above.
const ACPI_TABLES_BASE: u64 = LOW_METADATA_END + 0x1000;
/// The RSDP is pinned at the fixed 0xe0000 so the kernel's legacy RSDP scan of
/// `[0xe0000, 0x100000)` finds it, with no dependency on
/// `boot_params.acpi_rsdp_addr` (Linux 5.0+).
const RSDP_BASE: u64 = 0xe0000;
/// The x86 kernel brute-force scans the F-segment `[0xf0000, 0x100000)` for the
/// SMBIOS `_SM3_` DMI anchor, so the 24-byte entry point is pinned there. Its
/// 64-bit structure-table pointer lets the (potentially large) structure table
/// live in the low reserved area instead — see [`smbios_struct_table_base`].
const SMBIOS_FSEGMENT_BASE: u64 = 0xf0000;
/// The Linux x86 kernel loads at the conventional 1 MB mark.
const KERNEL_BASE: u64 = 0x100000;

/// Enables allocation of the SEV-SNP Linux boot protocol pages.
#[derive(Debug, Clone, Copy)]
pub struct SnpBootConfig {
    /// The page-table bit that marks private memory.
    pub c_bit: u8,
}

const SNP_BOOT_PAGE_COUNT: u64 = 4;

/// The GPA of the SMBIOS structure table: immediately above the ACPI tables in
/// the low reserved area. Only the `_SM3_` anchor stays in the F-segment; the
/// structure table lives here, reachable via the anchor's 64-bit pointer, so it
/// can grow well past the 64 KiB F-segment.
fn smbios_struct_table_base(acpi_tables_len: usize) -> u64 {
    ACPI_TABLES_BASE + align_up_to_page_size(acpi_tables_len as u64)
}

// Compile-time check that the fixed low-memory layout constants are ordered and
// non-overlapping. A violation here is a code bug, caught at build time.
const _: () = {
    assert!(GDT_BASE < ZERO_PAGE_BASE);
    assert!(ZERO_PAGE_BASE < CMDLINE_BASE);
    assert!(CMDLINE_BASE < CR3_BASE);
    assert!(ACPI_TABLES_BASE < RSDP_BASE);
    assert!(RSDP_BASE < SMBIOS_FSEGMENT_BASE);
    assert!(SMBIOS_FSEGMENT_BASE < KERNEL_BASE);
};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum InitrdAddressType {
    /// Load the initrd after the kernel at the next 2MB aligned address.
    AfterKernel,
    /// Load the initrd at the specified address.
    Address(u64),
}

pub struct InitrdConfig<'a> {
    pub initrd_address: InitrdAddressType,
    pub initrd: &'a mut dyn ReadSeek,
    pub size: u64,
}

/// Information returned about the kernel loaded.
#[derive(Debug, Default)]
pub struct KernelInfo {
    /// The base gpa the kernel was loaded at.
    pub gpa: u64,
    /// The size in bytes of the region the kernel was loaded at.
    pub size: u64,
    /// The gpa of the entrypoint of the kernel.
    pub entrypoint: u64,
}

/// Information returned about the initrd loaded.
#[derive(Debug, Default)]
pub struct InitrdInfo {
    /// The gpa the initrd was loaded at.
    pub gpa: u64,
    /// The size in bytes of the initrd loaded. Note that the region imported is aligned up to page size.
    pub size: u64,
}

/// Information returned about where certain parts were loaded.
#[derive(Debug, Default)]
pub struct LoadInfo {
    /// The information about the kernel loaded.
    pub kernel: KernelInfo,
    /// The information about the initrd loaded.
    pub initrd: Option<InitrdInfo>,
    /// The information about the device tree blob loaded.
    pub dtb: Option<std::ops::Range<u64>>,
    /// If a bzImage was loaded, the original setup header from the image.
    /// This must be placed into the zero page so the kernel's startup code
    /// can read its own configuration.
    pub bzimage_setup_header: Option<defs::setup_header>,
}

fn import_snp_boot_pages(
    importer: &mut impl ImageLoad<X86Register>,
    range: MemoryRange,
) -> Result<u64, Error> {
    assert_eq!(range.len(), SNP_BOOT_PAGE_COUNT * HV_PAGE_SIZE);
    let secrets_address = range.start();
    let cpuid_address = range.start() + HV_PAGE_SIZE;
    let cc_blob_address = range.start() + 2 * HV_PAGE_SIZE;
    let cc_setup_data_address = range.start() + 3 * HV_PAGE_SIZE;

    importer
        .import_pages(
            secrets_address / HV_PAGE_SIZE,
            1,
            "linux-snp-secrets",
            BootPageAcceptance::SecretsPage,
            &[],
        )
        .map_err(Error::Importer)?;
    importer
        .import_pages(
            cpuid_address / HV_PAGE_SIZE,
            1,
            "linux-snp-cpuid",
            BootPageAcceptance::CpuidPage,
            &[],
        )
        .map_err(Error::Importer)?;

    let cc_blob = defs::cc_blob_sev_info {
        magic: defs::CC_BLOB_SEV_INFO_MAGIC,
        version: 0,
        _reserved: 0,
        secrets_phys: secrets_address,
        secrets_len: HV_PAGE_SIZE as u32,
        _rsvd1: 0,
        cpuid_phys: cpuid_address,
        cpuid_len: HV_PAGE_SIZE as u32,
        _rsvd2: 0,
    };
    importer
        .import_pages(
            cc_blob_address / HV_PAGE_SIZE,
            1,
            "linux-snp-cc-blob",
            BootPageAcceptance::Exclusive,
            cc_blob.as_bytes(),
        )
        .map_err(Error::Importer)?;

    let cc_blob_address = u32::try_from(cc_blob_address)
        .map_err(|_| Error::SnpCcBlobAddressTooHigh(cc_blob_address))?;
    let cc_setup_data = defs::cc_setup_data {
        header: defs::setup_data {
            next: 0,
            ty: defs::SETUP_CC_BLOB,
            len: (size_of::<defs::cc_setup_data>() - size_of::<defs::setup_data>()) as u32,
        },
        cc_blob_address,
        _padding: [0; 3],
    };
    importer
        .import_pages(
            cc_setup_data_address / HV_PAGE_SIZE,
            1,
            "linux-snp-cc-setup-data",
            BootPageAcceptance::Exclusive,
            cc_setup_data.as_bytes(),
        )
        .map_err(Error::Importer)?;

    Ok(cc_setup_data_address)
}

/// Check if an address is aligned to a page.
fn check_address_alignment(address: u64) -> Result<(), Error> {
    if !address.is_multiple_of(HV_PAGE_SIZE) {
        Err(Error::UnalignedAddress(address))
    } else {
        Ok(())
    }
}

/// Import initrd
fn import_initrd<R: GuestArch>(
    initrd: Option<InitrdConfig<'_>>,
    next_addr: u64,
    importer: &mut dyn ImageLoad<R>,
) -> Result<Option<InitrdInfo>, Error> {
    let initrd_info = match initrd {
        Some(cfg) => {
            let initrd_address = match cfg.initrd_address {
                InitrdAddressType::AfterKernel => align_up_to_large_page_size(next_addr),
                InitrdAddressType::Address(addr) => addr,
            };

            tracing::trace!(initrd_address, "loading initrd");
            check_address_alignment(initrd_address)?;

            ChunkBuf::new()
                .import_file_region(
                    importer,
                    ImportFileRegion {
                        file: cfg.initrd,
                        file_offset: 0,
                        file_length: cfg.size,
                        gpa: initrd_address,
                        memory_length: cfg.size,
                        acceptance: BootPageAcceptance::Exclusive,
                        tag: "linux-initrd",
                    },
                )
                .map_err(Error::ImportInitrd)?;

            Some(InitrdInfo {
                gpa: initrd_address,
                size: cfg.size,
            })
        }
        None => None,
    };
    Ok(initrd_info)
}

/// Load only a Linux kernel and optional initrd to VTL0.
/// This does not setup register state or any other config information.
///
/// The kernel image may be either an uncompressed ELF (`vmlinux`) or a
/// compressed bzImage. If a bzImage is detected, the bzImage payload is
/// loaded directly into guest memory and the kernel's own decompressor
/// runs at boot time.
///
/// # Arguments
///
/// * `importer` - The importer to use.
/// * `kernel_image` - Kernel image (uncompressed ELF or bzImage).
/// * `kernel_minimum_start_address` - The minimum address the kernel can load at.
///   It cannot contain an entrypoint or program headers that refer to memory below this address.
/// * `initrd` - The initrd config, optional.
pub fn load_kernel_and_initrd_x64<F>(
    importer: &mut dyn ImageLoad<X86Register>,
    kernel_image: &mut F,
    kernel_minimum_start_address: u64,
    initrd: Option<InitrdConfig<'_>>,
) -> Result<LoadInfo, Error>
where
    F: Read + Seek,
{
    tracing::trace!(kernel_minimum_start_address, "loading x86_64 kernel");

    if crate::bzimage::is_bzimage(kernel_image).map_err(Error::BzImage)? {
        tracing::info!("detected bzImage format, loading via Linux boot protocol");
        return load_bzimage(importer, kernel_image, kernel_minimum_start_address, initrd);
    }

    let elf_load_info = load_static_elf(
        importer,
        kernel_image,
        kernel_minimum_start_address,
        0,
        false,
        BootPageAcceptance::Exclusive,
        "linux-kernel",
    )
    .map_err(Error::ElfLoader)?;

    let crate::elf::LoadInfo {
        minimum_address_used: min_addr,
        next_available_address: next_addr,
        entrypoint,
    } = elf_load_info;
    tracing::trace!(min_addr, next_addr, entrypoint, "loaded kernel");

    let initrd_info = import_initrd(initrd, next_addr, importer)?;

    Ok(LoadInfo {
        kernel: KernelInfo {
            gpa: min_addr,
            size: next_addr - min_addr,
            entrypoint,
        },
        initrd: initrd_info,
        dtb: None,
        bzimage_setup_header: None,
    })
}

/// Load a bzImage by placing its payload directly into guest memory at the
/// load address and following the Linux boot protocol. The kernel's built-in
/// decompressor handles the rest at boot time.
fn load_bzimage(
    importer: &mut dyn ImageLoad<X86Register>,
    kernel_image: &mut (impl Read + Seek),
    kernel_start_address: u64,
    initrd: Option<InitrdConfig<'_>>,
) -> Result<LoadInfo, Error> {
    let info = crate::bzimage::parse_bzimage(kernel_image).map_err(Error::BzImage)?;

    check_address_alignment(kernel_start_address)?;

    let payload_offset = (info.setup_sects as u64 + 1) * 512;
    let payload_len = info.protected_mode_size;
    let payload_memory_len = align_up_to_page_size(payload_len);
    let entrypoint = kernel_start_address + info.entry_offset;

    tracing::info!(
        kernel_start_address = format_args!("{:#x}", kernel_start_address),
        payload_offset,
        payload_len,
        entrypoint = format_args!("{:#x}", entrypoint),
        "loading bzImage payload into guest memory"
    );

    ChunkBuf::new()
        .import_file_region(
            importer,
            ImportFileRegion {
                file: kernel_image,
                file_offset: payload_offset,
                file_length: payload_len,
                gpa: kernel_start_address,
                memory_length: payload_memory_len,
                acceptance: BootPageAcceptance::Exclusive,
                tag: "linux-kernel",
            },
        )
        .map_err(Error::ImportBzImage)?;

    // Place initrd after the kernel's init_size region to avoid being
    // overwritten during decompression.
    let next_addr = kernel_start_address + payload_memory_len;
    let pref_address: u64 = info.setup_header.pref_address.into();
    let init_end = kernel_start_address
        .max(pref_address)
        .saturating_add(info.init_size as u64);
    let next_addr = next_addr.max(init_end);
    let initrd_info = import_initrd(initrd, next_addr, importer)?;

    Ok(LoadInfo {
        kernel: KernelInfo {
            gpa: kernel_start_address,
            size: payload_memory_len,
            entrypoint,
        },
        initrd: initrd_info,
        dtb: None,
        bzimage_setup_header: Some(info.setup_header),
    })
}

/// Import the boot metadata, ACPI/SMBIOS tables, zero page, and initial
/// registers for a kernel already described by `load_info`.
///
/// Internal helper shared by [`load_x86`] and [`load_config_x86`]. All guest
/// addresses come from the module-level layout constants; callers supply only
/// the table contents.
fn import_config(
    importer: &mut impl ImageLoad<X86Register>,
    load_info: &LoadInfo,
    cmdline: &CString,
    mem_layout: &MemoryLayout,
    acpi: &AcpiTables,
    smbios: Option<&crate::smbios::BuiltSmbios>,
    snp_boot: Option<SnpBootConfig>,
) -> Result<(), Error> {
    // Only import the cmdline if it actually contains something.
    // TODO: This should use the IGVM parameter instead?
    let raw_cmdline = cmdline.as_bytes_with_nul();
    if raw_cmdline.len() as u64 > CR3_BASE - CMDLINE_BASE {
        return Err(Error::CommandLineTooLong(
            raw_cmdline.len(),
            CR3_BASE - CMDLINE_BASE,
        ));
    }
    if raw_cmdline.len() > 1 {
        let cmdline_size_pages = align_up_to_page_size(raw_cmdline.len() as u64) / HV_PAGE_SIZE;
        importer
            .import_pages(
                CMDLINE_BASE / HV_PAGE_SIZE,
                cmdline_size_pages,
                "linux-commandline",
                BootPageAcceptance::Exclusive,
                raw_cmdline,
            )
            .map_err(Error::Importer)?;
    }

    import_default_gdt(importer, GDT_BASE / HV_PAGE_SIZE).map_err(Error::Importer)?;
    let mut page_table_work_buffer: Vec<PageTable> =
        vec![PageTable::new_zeroed(); PAGE_TABLE_MAX_COUNT];
    let mut page_table: Vec<u8> = vec![0; PAGE_TABLE_MAX_BYTES];
    let mut page_table_builder = IdentityMapBuilder::new(
        CR3_BASE,
        IdentityMapSize::Size4Gb,
        page_table_work_buffer.as_mut_slice(),
        page_table.as_mut_slice(),
    )?;
    if let Some(snp_boot) = snp_boot {
        page_table_builder = page_table_builder.with_confidential_bit(snp_boot.c_bit.into());
    }
    let page_table = page_table_builder.build();
    assert!((page_table.len() as u64).is_multiple_of(HV_PAGE_SIZE));
    importer
        .import_pages(
            CR3_BASE / HV_PAGE_SIZE,
            page_table.len() as u64 / HV_PAGE_SIZE,
            "linux-pagetables",
            BootPageAcceptance::Exclusive,
            page_table,
        )
        .map_err(Error::Importer)?;

    if acpi.tables.is_empty() {
        return Err(Error::EmptyAcpiTables);
    }
    let acpi_tables_size_pages = align_up_to_page_size(acpi.tables.len() as u64) / HV_PAGE_SIZE;
    importer
        .import_pages(
            RSDP_BASE / HV_PAGE_SIZE,
            1,
            "linux-rsdp",
            BootPageAcceptance::Exclusive,
            &acpi.rsdp,
        )
        .map_err(Error::Importer)?;
    importer
        .import_pages(
            ACPI_TABLES_BASE / HV_PAGE_SIZE,
            acpi_tables_size_pages,
            "linux-acpi-tables",
            BootPageAcceptance::Exclusive,
            &acpi.tables,
        )
        .map_err(Error::Importer)?;

    let requested_page_count = snp_boot.map_or(0, |_| SNP_BOOT_PAGE_COUNT);
    let ZeroPageBuildResult {
        mut boot_params,
        additional_pages,
    } = build_zero_page(
        mem_layout,
        acpi.tables.len(),
        smbios.map_or(0, |s| s.structure_table.len()),
        requested_page_count,
        cmdline,
        load_info.initrd.as_ref().map(|info| info.gpa).unwrap_or(0) as u32,
        load_info.initrd.as_ref().map(|info| info.size).unwrap_or(0) as u32,
        load_info.bzimage_setup_header.as_ref(),
    )?;
    if let Some(allocated_range) = additional_pages {
        boot_params.hdr.setup_data = import_snp_boot_pages(importer, allocated_range)?.into();
    }
    importer
        .import_pages(
            ZERO_PAGE_BASE / HV_PAGE_SIZE,
            1,
            "linux-zeropage",
            BootPageAcceptance::Exclusive,
            boot_params.as_bytes(),
        )
        .map_err(Error::Importer)?;

    // Set common X64 registers. Segments already set by default gdt.
    let mut import_reg = |register| {
        importer
            .import_vp_register(register)
            .map_err(Error::Importer)
    };

    import_reg(X86Register::Cr0(x86defs::X64_CR0_PG | x86defs::X64_CR0_PE))?;
    import_reg(X86Register::Cr3(CR3_BASE))?;
    import_reg(X86Register::Cr4(x86defs::X64_CR4_PAE))?;
    import_reg(X86Register::Efer(
        x86defs::X64_EFER_SCE
            | x86defs::X64_EFER_LME
            | x86defs::X64_EFER_LMA
            | x86defs::X64_EFER_NXE,
    ))?;
    import_reg(X86Register::Pat(x86defs::X86X_MSR_DEFAULT_PAT))?;

    // Set rip to entry point and rsi to zero page.
    import_reg(X86Register::Rip(load_info.kernel.entrypoint))?;
    import_reg(X86Register::Rsi(ZERO_PAGE_BASE))?;

    // No firmware will set MTRR values for the BSP.  Replicate what UEFI does here.
    // (enable MTRRs, default MTRR is uncached, and set lowest 640KB as WB)
    import_reg(X86Register::MtrrDefType(0xc00))?;
    import_reg(X86Register::MtrrFix64k00000(0x0606060606060606))?;
    import_reg(X86Register::MtrrFix16k80000(0x0606060606060606))?;

    if let Some(smbios) = smbios {
        // The `_SM3_` entry point (anchor) goes in the F-segment for the
        // kernel's DMI scan; its 64-bit pointer targets the structure table in
        // the low reserved area just above the ACPI tables.
        let anchor_pages = align_up_to_page_size(smbios.entry_point.len() as u64) / HV_PAGE_SIZE;
        importer
            .import_pages(
                SMBIOS_FSEGMENT_BASE / HV_PAGE_SIZE,
                anchor_pages,
                "linux-smbios-anchor",
                BootPageAcceptance::Exclusive,
                &smbios.entry_point,
            )
            .map_err(Error::Importer)?;

        let table_base = smbios_struct_table_base(acpi.tables.len());
        let table_pages = align_up_to_page_size(smbios.structure_table.len() as u64) / HV_PAGE_SIZE;
        importer
            .import_pages(
                table_base / HV_PAGE_SIZE,
                table_pages,
                "linux-smbios-tables",
                BootPageAcceptance::Exclusive,
                &smbios.structure_table,
            )
            .map_err(Error::Importer)?;
    }

    Ok(())
}

/// Place the ACPI tables, SMBIOS tables, boot metadata, zero page, and initial
/// registers for a Linux kernel that has *already* been loaded into guest
/// memory (as described by `load_info`).
///
/// The loader owns the entire sub-1 MB memory map; callers supply only
/// contents, never addresses:
///
/// * `cmdline` - the kernel command line.
/// * `mem_layout` - the guest memory layout, used to build the e820 map.
/// * `build_acpi` - a builder handed the nominal RSDP address; it must return
///   ACPI tables self-consistent for placement one page above that address.
///   The loader re-homes the RSDP to the fixed 0xe0000 legacy-scan location.
/// * `smbios` - an optional SMBIOS identity; when present the loader assembles
///   the `_SM3_` entry point and structure table into the F-segment.
/// * `snp_boot` - optionally allocate SEV-SNP Linux boot protocol pages.
pub fn load_config_x86(
    importer: &mut impl ImageLoad<X86Register>,
    load_info: &LoadInfo,
    cmdline: &CString,
    mem_layout: &MemoryLayout,
    build_acpi: impl FnOnce(u64) -> AcpiTables,
    smbios: Option<crate::smbios::SmbiosTables<'_>>,
    snp_boot: Option<SnpBootConfig>,
) -> Result<(), Error> {
    // The builder lays out a nominal RSDP page at LOW_METADATA_END followed by
    // the tables it points to; we keep only the tables (placed at
    // ACPI_TABLES_BASE) and re-home the RSDP to the fixed scan location.
    let acpi_tables = build_acpi(LOW_METADATA_END);

    // Build the SMBIOS tables (if an identity was supplied) with the structure
    // table addressed at its low-area home: the `_SM3_` anchor's 64-bit pointer
    // references it there while the anchor itself lands in the F-segment for the
    // kernel's DMI scan. See `smbios_struct_table_base` / `import_config`.
    let smbios = smbios.map(|tables| {
        crate::smbios::build(&tables, smbios_struct_table_base(acpi_tables.tables.len()))
    });

    import_config(
        importer,
        load_info,
        cmdline,
        mem_layout,
        &acpi_tables,
        smbios.as_ref(),
        snp_boot,
    )
}

/// Load a Linux kernel into VTL0 and place all of its supporting structures.
///
/// Loads the kernel (uncompressed ELF or bzImage) and optional initrd at the
/// conventional 1 MB address, then delegates to [`load_config_x86`] to place
/// the ACPI/SMBIOS tables, boot metadata, zero page, and initial registers.
/// See [`load_config_x86`] for the `build_acpi`/`smbios` contract.
pub fn load_x86<F>(
    importer: &mut impl ImageLoad<X86Register>,
    kernel_image: &mut F,
    initrd: Option<InitrdConfig<'_>>,
    cmdline: &CString,
    mem_layout: &MemoryLayout,
    build_acpi: impl FnOnce(u64) -> AcpiTables,
    smbios: Option<crate::smbios::SmbiosTables<'_>>,
    snp_boot: Option<SnpBootConfig>,
) -> Result<LoadInfo, Error>
where
    F: Read + Seek,
{
    let load_info = load_kernel_and_initrd_x64(importer, kernel_image, KERNEL_BASE, initrd)?;
    load_config_x86(
        importer, &load_info, cmdline, mem_layout, build_acpi, smbios, snp_boot,
    )?;
    Ok(load_info)
}

open_enum::open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum Aarch64ImagePageSize: u64 {
        UNSPECIFIED = 0,
        PAGE4_K = 1,
        PAGE16_K = 2,
        PAGE64_K = 3,
    }

}

impl Aarch64ImagePageSize {
    const fn into_bits(self) -> u64 {
        self.0
    }

    const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }
}

/// Arm64 flat kernel `Image` flags.
#[bitfield(u64)]
struct Aarch64ImageFlags {
    /// Bit 0:	Kernel endianness.  1 if BE, 0 if LE.
    #[bits(1)]
    pub big_endian: bool,
    /// Bit 1-2:	Kernel Page size.
    ///           0 - Unspecified.
    ///           1 - 4K
    ///           2 - 16K
    ///           3 - 64K
    #[bits(2)]
    pub page_size: Aarch64ImagePageSize,
    /// Bit 3:	Kernel physical placement
    ///           0 - 2MB aligned base should be as close as possible
    ///               to the base of DRAM, since memory below it is not
    ///               accessible via the linear mapping
    ///           1 - 2MB aligned base may be anywhere in physical
    ///               memory
    #[bits(1)]
    pub any_start_address: bool,
    /// Bits 4-63:	Reserved.
    #[bits(60)]
    pub _padding: u64,
}

// Kernel boot protocol is specified in the Linux kernel
// Documentation/arm64/booting.txt.
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Aarch64ImageHeader {
    /// Executable code
    _code0: u32,
    /// Executable code
    _code1: u32,
    /// Image load offset, little endian
    text_offset: u64,
    /// Effective Image size, little endian
    image_size: u64,
    /// kernel flags, little endian
    flags: u64,
    /// reserved
    _res2: u64,
    /// reserved
    _res3: u64,
    /// reserved
    _res4: u64,
    /// Magic number, little endian, "ARM\x64"
    magic: [u8; 4],
    /// reserved (used for PE COFF offset)
    _res5: u32,
}

const AARCH64_MAGIC_NUMBER: &[u8] = b"ARM\x64";

/// Load only an arm64 the flat Linux kernel `Image` and optional initrd.
/// This does not setup register state or any other config information.
///
/// # Arguments
///
/// * `importer` - The importer to use.
/// * `kernel_image` - Uncompressed ELF image for the kernel.
/// * `kernel_minimum_start_address` - The minimum address the kernel can load at.
///   It cannot contain an entrypoint or program headers that refer to memory below this address.
/// * `initrd` - The initrd config, optional.
/// * `device_tree_blob` - The device tree blob, optional.
pub fn load_kernel_and_initrd_arm64<F>(
    importer: &mut dyn ImageLoad<Aarch64Register>,
    kernel_image: &mut F,
    kernel_minimum_start_address: u64,
    initrd: Option<InitrdConfig<'_>>,
    device_tree_blob: Option<&[u8]>,
) -> Result<LoadInfo, Error>
where
    F: Read + Seek,
{
    tracing::trace!(kernel_minimum_start_address, "loading aarch64 kernel");

    assert_eq!(
        kernel_minimum_start_address & ((1 << 21) - 1),
        0,
        "Start offset must be aligned on the 2MiB boundary"
    );

    kernel_image
        .seek(SeekFrom::Start(0))
        .map_err(|_| Error::FlatLoader(FlatLoaderError::SeekKernelStart))?;

    let mut header = Aarch64ImageHeader::new_zeroed();
    kernel_image
        .read_exact(header.as_mut_bytes())
        .map_err(|_| Error::FlatLoader(FlatLoaderError::ReadKernelImage))?;

    tracing::debug!("aarch64 kernel header {header:x?}");

    if header.magic != AARCH64_MAGIC_NUMBER {
        return Err(Error::FlatLoader(FlatLoaderError::BadImageMagic));
    }

    let flags = Aarch64ImageFlags::from(header.flags);
    if flags.big_endian() {
        return Err(Error::FlatLoader(FlatLoaderError::BigEndianKernelImage));
    }
    if flags.page_size() != Aarch64ImagePageSize::PAGE4_K {
        return Err(Error::FlatLoader(
            FlatLoaderError::FourKibPageImageIsRequired,
        ));
    }
    if !flags.any_start_address() {
        return Err(Error::FlatLoader(FlatLoaderError::LowMemoryKernel));
    }

    // The `Image` must be placed `text_offset` bytes from a 2MB aligned base
    // address anywhere in usable system RAM and called there.

    kernel_image
        .seek(SeekFrom::Start(0))
        .map_err(|_| Error::FlatLoader(FlatLoaderError::SeekKernelStart))?;

    let mut image = Vec::new();
    kernel_image
        .read_to_end(&mut image)
        .map_err(|_| Error::FlatLoader(FlatLoaderError::ReadKernelImage))?;

    let kernel_load_offset = (kernel_minimum_start_address + header.text_offset) as usize;
    let kernel_size = if header.image_size != 0 {
        header.image_size
    } else {
        image.len() as u64
    };

    let kernel_size = align_up_to_page_size(kernel_size);
    importer
        .import_pages(
            kernel_load_offset as u64 / HV_PAGE_SIZE,
            kernel_size / HV_PAGE_SIZE,
            "linux-kernel",
            BootPageAcceptance::Exclusive,
            &image,
        )
        .map_err(Error::Importer)?;

    let next_addr = kernel_load_offset as u64 + kernel_size;

    let (next_addr, dtb) = if let Some(device_tree_blob) = device_tree_blob {
        let dtb_addr = align_up_to_page_size(next_addr);
        tracing::trace!(dtb_addr, "loading device tree blob at {dtb_addr:x?}");

        check_address_alignment(dtb_addr)?;
        let dtb_size_pages = align_up_to_page_size(device_tree_blob.len() as u64) / HV_PAGE_SIZE;

        importer
            .import_pages(
                dtb_addr / HV_PAGE_SIZE,
                dtb_size_pages,
                "linux-device-tree",
                BootPageAcceptance::Exclusive,
                device_tree_blob,
            )
            .map_err(Error::Importer)?;

        (
            dtb_addr + device_tree_blob.len() as u64,
            Some(dtb_addr..dtb_addr + device_tree_blob.len() as u64),
        )
    } else {
        (next_addr, None)
    };

    let initrd_info = import_initrd(initrd, next_addr, importer)?;

    Ok(LoadInfo {
        kernel: KernelInfo {
            gpa: kernel_minimum_start_address,
            size: kernel_size,
            entrypoint: kernel_load_offset as u64,
        },
        initrd: initrd_info,
        dtb,
        bzimage_setup_header: None,
    })
}

/// Load the configuration info and registers for the Linux kernel based on the provided LoadInfo.
/// Parameters:
/// * `importer` - The importer to use.
/// * `load_info` - The kernel load info that contains information on where the kernel and initrd are.
/// * `vtl` - The target VTL.
pub fn set_direct_boot_registers_arm64(
    importer: &mut impl ImageLoad<Aarch64Register>,
    load_info: &LoadInfo,
) -> Result<(), Error> {
    let mut import_reg = |register| {
        importer
            .import_vp_register(register)
            .map_err(Error::Importer)
    };

    import_reg(Aarch64Register::Pc(load_info.kernel.entrypoint))?;
    import_reg(Aarch64Register::Cpsr(
        Cpsr64::new()
            .with_sp(true)
            .with_el(1)
            .with_f(true)
            .with_i(true)
            .with_a(true)
            .with_d(true)
            .into(),
    ))?;
    import_reg(Aarch64Register::SctlrEl1(
        SctlrEl1::new()
            // MMU is disabled for EL1&0 stage 1 address translation.
            // The family of the `at` instructions and the `PAR_EL1` register are
            // useful for debugging MMU issues when it's on.
            .with_m(false)
            // Stage 1 Cacheability control, for data accesses.
            .with_c(true)
            // Stage 1 Cacheability control, for code.
            .with_i(true)
            // Reserved flags, must be set
            .with_eos(true)
            .with_tscxt(true)
            .with_eis(true)
            .with_span(true)
            .with_n_tlsmd(true)
            .with_lsmaoe(true)
            .into(),
    ))?;
    import_reg(Aarch64Register::TcrEl1(
        TranslationControlEl1::new()
            .with_t0sz(0x11)
            .with_irgn0(1)
            .with_orgn0(1)
            .with_sh0(3)
            .with_tg0(TranslationGranule0::TG_4KB)
            // Disable TTBR0_EL1 walks (i.e. the lower half).
            .with_epd0(1)
            // Disable TTBR1_EL1 walks (i.e. the upper half).
            .with_epd1(1)
            // Due to erratum #822227, need to set a valid TG1 regardless of EPD1.
            .with_tg1(TranslationGranule1::TG_4KB)
            .with_ips(IntermPhysAddrSize::IPA_48_BITS_256_TB)
            .into(),
    ))?;
    import_reg(Aarch64Register::Ttbr0El1(TranslationBaseEl1::new().into()))?;
    import_reg(Aarch64Register::Ttbr1El1(TranslationBaseEl1::new().into()))?;
    import_reg(Aarch64Register::VbarEl1(0))?;

    if let Some(dtb) = &load_info.dtb {
        import_reg(Aarch64Register::X0(dtb.start))?;
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::importer::IgvmParameterType;
    use crate::importer::IsolationConfig;
    use crate::importer::ParameterAreaIndex;
    use crate::importer::StartupMemoryType;
    use test_with_tracing::test;
    use zerocopy::FromBytes;

    const MB: u64 = 0x100000;
    const GB: u64 = 0x4000_0000;

    /// A guest memory layout with `ram_size` bytes of RAM and a 128 MB MMIO gap
    /// below 4 GiB (matching a typical x86_64 config). RAM up to the gap forms
    /// the first range `[0, min(ram_size, 4 GiB - 128 MB))`.
    fn make_layout(ram_size: u64) -> MemoryLayout {
        MemoryLayout::new(
            ram_size,
            &[MemoryRange::new(4 * GB - 128 * MB..4 * GB)],
            &[],
            &[],
            None,
        )
        .unwrap()
    }

    /// Asserts that `map[..entries]` exactly covers `[0, first_ram_end)` with no
    /// gaps or overlaps and strictly ascending addresses, and that no entry is
    /// empty. The first entry must start at 0 and the last must end precisely at
    /// `first_ram_end`.
    fn assert_contiguous(p: &defs::boot_params, first_ram_end: u64) {
        let entries = p.e820_entries as usize;
        assert!(entries > 0);
        let mut expected_addr = 0u64;
        for i in 0..entries {
            let e = &p.e820_map[i];
            assert_ne!(u64::from(e.size), 0, "entry {i} is empty");
            assert_eq!(u64::from(e.addr), expected_addr, "gap/overlap at entry {i}");
            expected_addr = u64::from(e.addr) + u64::from(e.size);
        }
        assert_eq!(
            expected_addr, first_ram_end,
            "map ends at {expected_addr:#x}, expected {first_ram_end:#x}"
        );
    }

    #[test]
    fn zero_page_layout_with_smbios() {
        let acpi_len = 0x1800; // aligns up to 0x2000
        let smbios_len = 0x100; // aligns up to 0x1000
        let p = build_zero_page(
            &make_layout(256 * MB),
            acpi_len,
            smbios_len,
            0,
            &CString::new("root=/dev/sda").unwrap(),
            0,
            0,
            None,
        )
        .unwrap()
        .boot_params;

        let acpi_end = ACPI_TABLES_BASE + 0x2000;
        let smbios_end = acpi_end + 0x1000;
        let expected = [
            (0, ACPI_TABLES_BASE, defs::E820_RAM),
            (ACPI_TABLES_BASE, 0x2000, defs::E820_ACPI),
            (acpi_end, 0x1000, defs::E820_RESERVED),
            (smbios_end, RSDP_BASE - smbios_end, defs::E820_RAM),
            (RSDP_BASE, 0x100000 - RSDP_BASE, defs::E820_RESERVED),
            (0x100000, 256 * MB - 0x100000, defs::E820_RAM),
        ];
        assert_eq!(p.e820_entries as usize, expected.len());
        for (i, (addr, size, typ)) in expected.iter().enumerate() {
            let e = &p.e820_map[i];
            assert_eq!(u64::from(e.addr), *addr, "entry {i} addr");
            assert_eq!(u64::from(e.size), *size, "entry {i} size");
            assert_eq!(u32::from(e.typ), *typ, "entry {i} type");
        }
        assert_contiguous(&p, 256 * MB);
    }

    #[test]
    fn zero_page_skips_empty_smbios_region() {
        // With no SMBIOS structure table, the reserved SMBIOS region collapses
        // to zero length and must not appear as an empty e820 entry.
        let p = build_zero_page(
            &make_layout(256 * MB),
            0x1800,
            0,
            0,
            &CString::new("").unwrap(),
            0,
            0,
            None,
        )
        .unwrap()
        .boot_params;

        let acpi_end = ACPI_TABLES_BASE + 0x2000;
        let expected = [
            (0, ACPI_TABLES_BASE, defs::E820_RAM),
            (ACPI_TABLES_BASE, 0x2000, defs::E820_ACPI),
            (acpi_end, RSDP_BASE - acpi_end, defs::E820_RAM),
            (RSDP_BASE, 0x100000 - RSDP_BASE, defs::E820_RESERVED),
            (0x100000, 256 * MB - 0x100000, defs::E820_RAM),
        ];
        assert_eq!(p.e820_entries as usize, expected.len());
        for (i, (addr, size, typ)) in expected.iter().enumerate() {
            let e = &p.e820_map[i];
            assert_eq!(u64::from(e.addr), *addr, "entry {i} addr");
            assert_eq!(u64::from(e.size), *size, "entry {i} size");
            assert_eq!(u32::from(e.typ), *typ, "entry {i} type");
        }
        assert_contiguous(&p, 256 * MB);
    }

    #[test]
    fn zero_page_multiple_ram_ranges() {
        // 8 GiB of RAM splits around the 4 GiB MMIO gap into two ranges; the
        // second appears after the six fixed low-memory entries.
        let p = build_zero_page(
            &make_layout(8 * GB),
            0x1000,
            0x1000,
            0,
            &CString::new("").unwrap(),
            0,
            0,
            None,
        )
        .unwrap()
        .boot_params;
        assert_eq!(p.e820_entries, 7);
        let last = &p.e820_map[6];
        assert_eq!(u64::from(last.addr), 4 * GB);
        assert_eq!(u32::from(last.typ), defs::E820_RAM);
        // The six fixed low-memory entries are contiguous from 0; the second
        // RAM range sits above the 4 GiB MMIO gap, so contiguity legitimately
        // breaks there.
        let below_gap = &p.e820_map[5];
        assert_eq!(
            u64::from(below_gap.addr) + u64::from(below_gap.size),
            4 * GB - 128 * MB
        );
    }

    #[test]
    fn zero_page_tables_too_large() {
        // ACPI tables large enough to run past the RSDP reserved region.
        let result = build_zero_page(
            &make_layout(256 * MB),
            (RSDP_BASE - ACPI_TABLES_BASE) as usize + 0x1000,
            0,
            0,
            &CString::new("").unwrap(),
            0,
            0,
            None,
        );
        match result {
            Err(Error::LowTablesTooLarge(..)) => {}
            other => panic!("expected LowTablesTooLarge, got {:?}", other.err()),
        }
    }

    /// An importer that records `import_pages` placements and accepts registers,
    /// panicking on any other entry point (none of which the Linux config path
    /// exercises).
    #[derive(Default)]
    struct RecordingImporter {
        /// `(debug_tag, page_base, page_count)` for each imported region.
        pages: Vec<(String, u64, u64)>,
        imports: Vec<ImportRecord>,
    }

    #[derive(Debug)]
    struct ImportRecord {
        page_base: u64,
        page_count: u64,
        tag: String,
        acceptance: BootPageAcceptance,
        data: Vec<u8>,
    }

    impl RecordingImporter {
        fn page_base(&self, tag: &str) -> Option<u64> {
            self.pages
                .iter()
                .find(|(t, ..)| t == tag)
                .map(|(_, base, _)| *base)
        }
    }

    impl ImageLoad<X86Register> for RecordingImporter {
        fn isolation_config(&self) -> IsolationConfig {
            IsolationConfig {
                paravisor_present: false,
                isolation_type: crate::importer::IsolationType::None,
                shared_gpa_boundary_bits: None,
            }
        }

        fn create_parameter_area(
            &mut self,
            _page_base: u64,
            _page_count: u32,
            _debug_tag: &str,
        ) -> anyhow::Result<ParameterAreaIndex> {
            unimplemented!()
        }

        fn create_parameter_area_with_data(
            &mut self,
            _page_base: u64,
            _page_count: u32,
            _debug_tag: &str,
            _initial_data: &[u8],
        ) -> anyhow::Result<ParameterAreaIndex> {
            unimplemented!()
        }

        fn import_parameter(
            &mut self,
            _parameter_area: ParameterAreaIndex,
            _byte_offset: u32,
            _parameter_type: IgvmParameterType,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }

        fn import_pages(
            &mut self,
            page_base: u64,
            page_count: u64,
            debug_tag: &'static str,
            acceptance: BootPageAcceptance,
            data: &[u8],
        ) -> anyhow::Result<()> {
            self.pages
                .push((debug_tag.to_string(), page_base, page_count));
            self.imports.push(ImportRecord {
                page_base,
                page_count,
                tag: debug_tag.to_string(),
                acceptance,
                data: data.to_vec(),
            });
            Ok(())
        }

        fn import_vp_register(&mut self, _register: X86Register) -> anyhow::Result<()> {
            Ok(())
        }

        fn verify_startup_memory_available(
            &mut self,
            _page_base: u64,
            _page_count: u64,
            _memory_type: StartupMemoryType,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn set_vp_context_page(&mut self, _page_base: u64) -> anyhow::Result<()> {
            unimplemented!()
        }

        fn relocation_region(
            &mut self,
            _gpa: u64,
            _size_bytes: u64,
            _relocation_alignment: u64,
            _minimum_relocation_gpa: u64,
            _maximum_relocation_gpa: u64,
            _apply_rip_offset: bool,
            _apply_gdtr_offset: bool,
            _vp_index: u16,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }

        fn page_table_relocation(
            &mut self,
            _page_table_gpa: u64,
            _size_pages: u64,
            _used_pages: u64,
            _vp_index: u16,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }

        fn set_imported_regions_config_page(&mut self, _page_base: u64) {
            unimplemented!()
        }
    }

    fn test_load_info() -> LoadInfo {
        LoadInfo {
            kernel: KernelInfo {
                gpa: KERNEL_BASE,
                size: 0x1000,
                entrypoint: KERNEL_BASE,
            },
            initrd: None,
            dtb: None,
            bzimage_setup_header: None,
        }
    }

    #[test]
    fn import_config_places_tables_at_fixed_addresses() {
        let acpi = AcpiTables {
            rsdp: vec![0u8; 0x1000],
            tables: vec![0u8; 0x1800],
        };
        let smbios = crate::smbios::BuiltSmbios {
            entry_point: vec![0u8; crate::smbios::ENTRY_POINT_SIZE],
            structure_table: vec![0u8; 0x100],
        };
        let mut importer = RecordingImporter::default();
        import_config(
            &mut importer,
            &test_load_info(),
            &CString::new("console=ttyS0").unwrap(),
            &make_layout(256 * MB),
            &acpi,
            Some(&smbios),
            None,
        )
        .unwrap();

        // The RSDP is re-homed to the fixed legacy-scan address, while the
        // tables it points to stay at the loader's chosen base.
        assert_eq!(
            importer.page_base("linux-rsdp"),
            Some(RSDP_BASE / HV_PAGE_SIZE)
        );
        assert_eq!(
            importer.page_base("linux-acpi-tables"),
            Some(ACPI_TABLES_BASE / HV_PAGE_SIZE)
        );
        // The SMBIOS anchor lands in the F-segment; the structure table sits
        // just above the ACPI tables.
        assert_eq!(
            importer.page_base("linux-smbios-anchor"),
            Some(SMBIOS_FSEGMENT_BASE / HV_PAGE_SIZE)
        );
        assert_eq!(
            importer.page_base("linux-smbios-tables"),
            Some(smbios_struct_table_base(acpi.tables.len()) / HV_PAGE_SIZE)
        );
        // Boot metadata at its fixed low-memory homes.
        assert_eq!(
            importer.page_base("linux-zeropage"),
            Some(ZERO_PAGE_BASE / HV_PAGE_SIZE)
        );
        assert_eq!(
            importer.page_base("linux-commandline"),
            Some(CMDLINE_BASE / HV_PAGE_SIZE)
        );
        assert_eq!(
            importer.page_base("linux-pagetables"),
            Some(CR3_BASE / HV_PAGE_SIZE)
        );
    }

    #[test]
    fn import_config_rejects_oversized_command_line() {
        let acpi = AcpiTables {
            rsdp: vec![0u8; 0x1000],
            tables: vec![0u8; 0x1000],
        };
        // One byte too long once the NUL terminator is added.
        let cmdline = CString::new(vec![b'a'; (CR3_BASE - CMDLINE_BASE) as usize]).unwrap();
        let mut importer = RecordingImporter::default();
        let err = import_config(
            &mut importer,
            &test_load_info(),
            &cmdline,
            &make_layout(256 * MB),
            &acpi,
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, Error::CommandLineTooLong(..)), "got {err:?}");
        assert!(importer.pages.is_empty(), "importer used before the check");
    }

    #[test]
    fn import_config_sets_snp_c_bit_in_page_tables() {
        const C_BIT: u8 = 51;
        let acpi = AcpiTables {
            rsdp: vec![0u8; 0x1000],
            tables: vec![0u8; 0x1000],
        };
        let mut importer = RecordingImporter::default();
        import_config(
            &mut importer,
            &test_load_info(),
            &CString::new("").unwrap(),
            &make_layout(256 * MB),
            &acpi,
            None,
            Some(SnpBootConfig { c_bit: C_BIT }),
        )
        .unwrap();

        let page_tables = importer
            .imports
            .iter()
            .find(|import| import.tag == "linux-pagetables")
            .unwrap();
        for entry in page_tables.data.chunks_exact(8).map(|entry| {
            u64::from_ne_bytes(entry.try_into().expect("page table entry is eight bytes"))
        }) {
            if entry & 1 != 0 {
                assert_ne!(entry & (1 << C_BIT), 0);
            }
        }
    }

    #[test]
    fn import_config_rejects_empty_acpi_tables() {
        // Empty tables would import zero pages; the loader must reject them
        // rather than feed page_count == 0 into the importer.
        let acpi = AcpiTables {
            rsdp: vec![0u8; 0x1000],
            tables: Vec::new(),
        };
        let mut importer = RecordingImporter::default();
        let err = import_config(
            &mut importer,
            &test_load_info(),
            &CString::new("").unwrap(),
            &make_layout(256 * MB),
            &acpi,
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, Error::EmptyAcpiTables), "got {err:?}");
    }

    #[test]
    fn imports_snp_boot_pages_with_linux_cc_blob() {
        let acpi_len = 0x1800;
        let smbios_len = 0x100;
        let ZeroPageBuildResult {
            boot_params,
            additional_pages,
        } = build_zero_page(
            &make_layout(256 * MB),
            acpi_len,
            smbios_len,
            SNP_BOOT_PAGE_COUNT,
            &CString::new("").unwrap(),
            0,
            0,
            None,
        )
        .unwrap();
        let allocated_range = additional_pages.unwrap();
        let mut importer = RecordingImporter::default();

        let cc_setup_data_address = import_snp_boot_pages(&mut importer, allocated_range).unwrap();

        assert_eq!(importer.imports.len(), 4);
        assert_eq!(
            allocated_range.start(),
            ACPI_TABLES_BASE
                + align_up_to_page_size(acpi_len as u64)
                + align_up_to_page_size(smbios_len as u64)
        );
        assert_eq!(
            importer.imports[0].page_base,
            allocated_range.start() / HV_PAGE_SIZE
        );
        assert_eq!(importer.imports[0].page_count, 1);
        assert_eq!(importer.imports[0].tag, "linux-snp-secrets");
        assert_eq!(
            importer.imports[0].acceptance,
            BootPageAcceptance::SecretsPage
        );
        assert_eq!(
            importer.imports[1].page_base,
            allocated_range.start() / HV_PAGE_SIZE + 1
        );
        assert_eq!(
            importer.imports[1].acceptance,
            BootPageAcceptance::CpuidPage
        );

        let cc_blob = defs::cc_blob_sev_info::read_from_bytes(&importer.imports[2].data).unwrap();
        assert_eq!(cc_blob.magic, defs::CC_BLOB_SEV_INFO_MAGIC);
        assert_eq!(cc_blob.version, 0);
        assert_eq!(cc_blob.secrets_phys, allocated_range.start());
        assert_eq!(cc_blob.secrets_len, HV_PAGE_SIZE as u32);
        assert_eq!(cc_blob.cpuid_phys, allocated_range.start() + HV_PAGE_SIZE);
        assert_eq!(cc_blob.cpuid_len, HV_PAGE_SIZE as u32);

        let cc_setup_data =
            defs::cc_setup_data::read_from_bytes(&importer.imports[3].data).unwrap();
        assert_eq!(cc_setup_data.header.next, 0);
        assert_eq!(cc_setup_data.header.ty, defs::SETUP_CC_BLOB);
        assert_eq!(
            cc_setup_data.header.len,
            (size_of::<defs::cc_setup_data>() - size_of::<defs::setup_data>()) as u32
        );
        assert_eq!(
            cc_setup_data.cc_blob_address,
            u32::try_from(allocated_range.start() + 2 * HV_PAGE_SIZE).unwrap()
        );
        assert_eq!(allocated_range.end(), cc_setup_data_address + HV_PAGE_SIZE,);
        assert!(allocated_range.end() <= RSDP_BASE);

        let smbios_reserved = &boot_params.e820_map[2];
        let acpi_end = ACPI_TABLES_BASE + align_up_to_page_size(acpi_len as u64);
        assert_eq!(u64::from(smbios_reserved.addr), acpi_end);
        assert_eq!(
            u64::from(smbios_reserved.size),
            align_up_to_page_size(smbios_len as u64)
        );
        let additional_reserved = &boot_params.e820_map[3];
        assert_eq!(u64::from(additional_reserved.addr), allocated_range.start());
        assert_eq!(u64::from(additional_reserved.size), allocated_range.len());

        assert!(matches!(
            build_zero_page(
                &make_layout(256 * MB),
                (RSDP_BASE - ACPI_TABLES_BASE) as usize,
                0,
                SNP_BOOT_PAGE_COUNT,
                &CString::new("").unwrap(),
                0,
                0,
                None,
            ),
            Err(Error::LowTablesTooLarge(..))
        ));
    }
}
