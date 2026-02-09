// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux specific loader definitions and implementation.

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
use page_table::IdentityMapSize;
use page_table::x64::IdentityMapBuilder;
use page_table::x64::PAGE_TABLE_MAX_BYTES;
use page_table::x64::PAGE_TABLE_MAX_COUNT;
use page_table::x64::PageTable;
use page_table::x64::align_up_to_large_page_size;
use page_table::x64::align_up_to_page_size;
use std::ffi::CString;
use thiserror::Error;
use vm_topology::memory::MemoryLayout;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Construct a zero page from the following parameters.
/// TODO: support different acpi_base other than 0xe0000
pub fn build_zero_page(
    mem_layout: &MemoryLayout,
    acpi_base: u64,
    acpi_len: usize,
    cmdline_config: &CommandLineConfig<'_>,
    initrd_base: u32,
    initrd_size: u32,
) -> defs::boot_params {
    let mut p = defs::boot_params {
        hdr: defs::setup_header {
            type_of_loader: 0xff,
            boot_flag: 0xaa55.into(),
            header: 0x53726448.into(),
            cmd_line_ptr: cmdline_config.address.try_into().expect("must fit in u32"),
            cmdline_size: (cmdline_config.cmdline.as_bytes().len() as u64)
                .try_into()
                .expect("must fit in u32"),
            ramdisk_image: initrd_base.into(),
            ramdisk_size: initrd_size.into(),
            kernel_alignment: 0x100000.into(),
            ..FromZeros::new_zeroed()
        },
        ..FromZeros::new_zeroed()
    };

    let mut ram = mem_layout.ram().iter().cloned();
    let range = ram.next().expect("at least one ram range");
    assert_eq!(range.range.start(), 0);
    assert!(range.range.end() >= 0x100000);
    // TODO: support better e820 building, for now acpi_base must be 0xe0000
    assert_eq!(acpi_base, 0xe0000);
    p.e820_map[0] = defs::e820entry {
        addr: 0.into(),
        size: 0xe0000.into(),
        typ: defs::E820_RAM.into(),
    };
    let aligned_acpi_len = (acpi_len + 0xfff) & !0xfff;
    p.e820_map[1] = defs::e820entry {
        addr: 0xe0000.into(),
        size: (aligned_acpi_len as u64).into(),
        typ: defs::E820_ACPI.into(),
    };
    p.e820_map[2] = defs::e820entry {
        addr: (0xe0000 + aligned_acpi_len as u64).into(),
        size: (range.range.end() - 0xe0000 - aligned_acpi_len as u64).into(),
        typ: defs::E820_RAM.into(),
    };
    let mut n = 3;
    for range in ram {
        p.e820_map[n] = defs::e820entry {
            addr: range.range.start().into(),
            size: range.range.len().into(),
            typ: defs::E820_RAM.into(),
        };
        n += 1;
    }
    p.e820_entries = n as u8;

    p
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
    #[error("flat loader error")]
    FlatLoader(#[source] FlatLoaderError),
    #[error("Address is not page aligned")]
    UnalignedAddress(u64),
    #[error("importer error")]
    Importer(#[source] anyhow::Error),
    #[error("PageTableBuilder: {0}")]
    PageTableBuilder(#[from] page_table::Error),
}

pub struct AcpiConfig<'a> {
    pub rdsp_address: u64,
    pub rdsp: &'a [u8],
    pub tables_address: u64,
    pub tables: &'a [u8],
}

pub struct ZeroPageConfig<'a> {
    /// The address to load the zero page at.
    pub address: u64,
    /// The memory layout used to build the e820 map.
    pub mem_layout: &'a MemoryLayout,
    /// The base address acpi tables are loaded at.
    pub acpi_base_address: u64,
    /// The overall size of acpi tables.
    pub acpi_len: usize,
}

pub struct CommandLineConfig<'a> {
    pub address: u64,
    pub cmdline: &'a CString,
}

pub struct RegisterConfig {
    pub gdt_address: u64,
    pub page_table_address: u64,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum InitrdAddressType {
    /// Load the initrd after the kernel at the next 2MB aligned address.
    AfterKernel,
    /// Load the initrd at the specified address.
    Address(u64),
}

pub struct InitrdConfig<'a> {
    pub initrd_address: InitrdAddressType,
    pub initrd: &'a [u8],
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
    let initrd_info = match &initrd {
        Some(cfg) => {
            let initrd_address = match cfg.initrd_address {
                InitrdAddressType::AfterKernel => align_up_to_large_page_size(next_addr),
                InitrdAddressType::Address(addr) => addr,
            };

            tracing::trace!(initrd_address, "loading initrd");
            check_address_alignment(initrd_address)?;
            let initrd_size_pages = align_up_to_page_size(cfg.initrd.len() as u64) / HV_PAGE_SIZE;
            importer
                .import_pages(
                    initrd_address / HV_PAGE_SIZE,
                    initrd_size_pages,
                    "linux-initrd",
                    BootPageAcceptance::Exclusive,
                    cfg.initrd,
                )
                .map_err(Error::Importer)?;

            Some(InitrdInfo {
                gpa: initrd_address,
                size: cfg.initrd.len() as u64,
            })
        }
        None => None,
    };
    Ok(initrd_info)
}

/// Load only a Linux kernel and optional initrd to VTL0.
/// This does not setup register state or any other config information.
///
/// # Arguments
///
/// * `importer` - The importer to use.
/// * `kernel_image` - Uncompressed ELF image for the kernel.
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
    F: std::io::Read + std::io::Seek,
{
    tracing::trace!(kernel_minimum_start_address, "loading x86_64 kernel");
    let crate::elf::LoadInfo {
        minimum_address_used: min_addr,
        next_available_address: next_addr,
        entrypoint,
    } = load_static_elf(
        importer,
        kernel_image,
        kernel_minimum_start_address,
        0,
        false,
        BootPageAcceptance::Exclusive,
        "linux-kernel",
    )
    .map_err(Error::ElfLoader)?;
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
    })
}

/// Load the configuration info and registers for the Linux kernel based on the provided LoadInfo.
///
/// # Arguments
/// * `importer` - The importer to use.
/// * `load_info` - The kernel load info that contains information on where the kernel and initrd are.
/// * `command_line` - The kernel command line.
/// * `zero_page` - The kernel zero page.
/// * `registers` - X86Register config.
pub fn load_config(
    importer: &mut impl ImageLoad<X86Register>,
    load_info: &LoadInfo,
    command_line: CommandLineConfig<'_>,
    zero_page: ZeroPageConfig<'_>,
    acpi: AcpiConfig<'_>,
    registers: RegisterConfig,
) -> Result<(), Error> {
    tracing::trace!(command_line.address);
    // Only import the cmdline if it actually contains something.
    // TODO: This should use the IGVM parameter instead?
    let raw_cmdline = command_line.cmdline.as_bytes_with_nul();
    if raw_cmdline.len() > 1 {
        check_address_alignment(command_line.address)?;
        let cmdline_size_pages = align_up_to_page_size(raw_cmdline.len() as u64) / HV_PAGE_SIZE;
        importer
            .import_pages(
                command_line.address / HV_PAGE_SIZE,
                cmdline_size_pages,
                "linux-commandline",
                BootPageAcceptance::Exclusive,
                raw_cmdline,
            )
            .map_err(Error::Importer)?;
    }

    check_address_alignment(registers.gdt_address)?;
    import_default_gdt(importer, registers.gdt_address / HV_PAGE_SIZE).map_err(Error::Importer)?;
    check_address_alignment(registers.page_table_address)?;
    let mut page_table_work_buffer: Vec<PageTable> =
        vec![PageTable::new_zeroed(); PAGE_TABLE_MAX_COUNT];
    let mut page_table: Vec<u8> = vec![0; PAGE_TABLE_MAX_BYTES];
    let page_table_builder = IdentityMapBuilder::new(
        registers.page_table_address,
        IdentityMapSize::Size4Gb,
        page_table_work_buffer.as_mut_slice(),
        page_table.as_mut_slice(),
    )?;
    let page_table = page_table_builder.build();
    assert!((page_table.len() as u64).is_multiple_of(HV_PAGE_SIZE));
    importer
        .import_pages(
            registers.page_table_address / HV_PAGE_SIZE,
            page_table.len() as u64 / HV_PAGE_SIZE,
            "linux-pagetables",
            BootPageAcceptance::Exclusive,
            page_table,
        )
        .map_err(Error::Importer)?;

    // NOTE: A whole page is given to the RDSP for simplicity.
    check_address_alignment(acpi.rdsp_address)?;
    check_address_alignment(acpi.tables_address)?;
    let acpi_tables_size_pages = align_up_to_page_size(acpi.tables.len() as u64) / HV_PAGE_SIZE;
    importer
        .import_pages(
            acpi.rdsp_address / HV_PAGE_SIZE,
            1,
            "linux-rdsp",
            BootPageAcceptance::Exclusive,
            acpi.rdsp,
        )
        .map_err(Error::Importer)?;
    importer
        .import_pages(
            acpi.tables_address / HV_PAGE_SIZE,
            acpi_tables_size_pages,
            "linux-acpi-tables",
            BootPageAcceptance::Exclusive,
            acpi.tables,
        )
        .map_err(Error::Importer)?;

    check_address_alignment(zero_page.address)?;
    let boot_params = build_zero_page(
        zero_page.mem_layout,
        zero_page.acpi_base_address,
        zero_page.acpi_len,
        &command_line,
        load_info.initrd.as_ref().map(|info| info.gpa).unwrap_or(0) as u32,
        load_info.initrd.as_ref().map(|info| info.size).unwrap_or(0) as u32,
    );
    importer
        .import_pages(
            zero_page.address / HV_PAGE_SIZE,
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
    import_reg(X86Register::Cr3(registers.page_table_address))?;
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
    import_reg(X86Register::Rsi(zero_page.address))?;

    // No firmware will set MTRR values for the BSP.  Replicate what UEFI does here.
    // (enable MTRRs, default MTRR is uncached, and set lowest 640KB as WB)
    import_reg(X86Register::MtrrDefType(0xc00))?;
    import_reg(X86Register::MtrrFix64k00000(0x0606060606060606))?;
    import_reg(X86Register::MtrrFix16k80000(0x0606060606060606))?;

    Ok(())
}

/// Load a Linux kernel into VTL0.
///
/// # Arguments
///
/// * `importer` - The importer to use.
/// * `kernel_image` - Uncompressed ELF image for the kernel.
/// * `kernel_minimum_start_address` - The minimum address the kernel can load at.
///   It cannot contain an entrypoint or program headers that refer to memory below this address.
/// * `initrd` - The initrd config, optional.
/// * `command_line` - The kernel command line.
/// * `zero_page` - The kernel zero page.
/// * `acpi` - The acpi config.
/// * `registers` - X86Register config.
pub fn load_x86<F>(
    importer: &mut impl ImageLoad<X86Register>,
    kernel_image: &mut F,
    kernel_minimum_start_address: u64,
    initrd: Option<InitrdConfig<'_>>,
    command_line: CommandLineConfig<'_>,
    zero_page: ZeroPageConfig<'_>,
    acpi: AcpiConfig<'_>,
    registers: RegisterConfig,
) -> Result<LoadInfo, Error>
where
    F: std::io::Read + std::io::Seek,
{
    let load_info =
        load_kernel_and_initrd_x64(importer, kernel_image, kernel_minimum_start_address, initrd)?;

    load_config(
        importer,
        &load_info,
        command_line,
        zero_page,
        acpi,
        registers,
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
    F: std::io::Read + std::io::Seek,
{
    tracing::trace!(kernel_minimum_start_address, "loading aarch64 kernel");

    assert_eq!(
        kernel_minimum_start_address & ((1 << 21) - 1),
        0,
        "Start offset must be aligned on the 2MiB boundary"
    );

    kernel_image
        .seek(std::io::SeekFrom::Start(0))
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
        .seek(std::io::SeekFrom::Start(0))
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
