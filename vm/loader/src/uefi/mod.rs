// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI specific loader definitions and implementation.

pub mod config;

#[cfg(guest_arch = "aarch64")]
use aarch64 as arch;
#[cfg(guest_arch = "x86_64")]
use x86_64 as arch;

pub use arch::CONFIG_BLOB_GPA_BASE;
pub use arch::IMAGE_SIZE;
pub use arch::load;

use guid::Guid;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// Constant defining the offset within the image where the SEC volume starts.
// TODO: Revisit this when we reorganize the firmware layout. One option
// would be to just put the SEC volume at the start of the image, so no need
// for this offset.
const SEC_FIRMWARE_VOLUME_OFFSET: u64 = 0x005E0000;

/// Expand a 3 byte sequence into little-endian integer.
fn expand_3byte_integer(size: [u8; 3]) -> u64 {
    ((size[2] as u64) << 16) + ((size[1] as u64) << 8) + size[0] as u64
}

const fn signature_16(v: &[u8; 2]) -> u16 {
    v[0] as u16 | (v[1] as u16) << 8
}

const fn signature_32(v: &[u8; 4]) -> u32 {
    v[0] as u32 | (v[1] as u32) << 8 | (v[2] as u32) << 16 | (v[3] as u32) << 24
}

const IMAGE_DOS_SIGNATURE: u16 = 0x5A4D; // MZ
const IMAGE_NT_SIGNATURE: u32 = 0x00004550; // PE00
const TE_IMAGE_HEADER_SIGNATURE: u16 = signature_16(b"VZ");
const EFI_FVH_SIGNATURE: u32 = signature_32(b"_FVH");

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct ImageDosHeader {
    e_magic: u16,      // Magic number
    e_cblp: u16,       // Bytes on last page of file
    e_cp: u16,         // Pages in file
    e_crlc: u16,       // Relocations
    e_cparhdr: u16,    // Size of header in paragraphs
    e_minalloc: u16,   // Minimum extra paragraphs needed
    e_maxalloc: u16,   // Maximum extra paragraphs needed
    e_ss: u16,         // Initial (relative) SS value
    e_sp: u16,         // Initial SP value
    e_csum: u16,       // Checksum
    e_ip: u16,         // Initial IP value
    e_cs: u16,         // Initial (relative) CS value
    e_lfarlc: u16,     // File address of relocation table
    e_ovno: u16,       // Overlay number
    e_res: [u16; 4],   // Reserved words
    e_oemid: u16,      // OEM identifier (for e_oeminfo)
    e_oeminfo: u16,    // OEM information; e_oemid specific
    e_res2: [u16; 10], // Reserved words
    e_lfanew: i32,     // File address of new exe header
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct TeImageHeader {
    signature: u16,
    machine: u16,
    number_of_sections: u8,
    subsystem: u8,
    stripped_size: u16,
    address_of_entry_point: u32,
    base_of_code: u32,
    image_base: u64,
    data_directory: [ImageDataDirectory; 2],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct ImageNtHeaders32 {
    signature: u32,
    file_header: ImageFileHeader,
    optional_header: ImageOptionalHeader32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct ImageFileHeader {
    machine: u16,
    number_of_sections: u16,
    time_date_stamp: u32,
    pointer_to_symbol_table: u32,
    number_of_symbols: u32,
    size_of_optional_header: u16,
    characteristics: u16,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct ImageOptionalHeader32 {
    magic: u16,
    major_linker_version: u8,
    minor_linker_version: u8,
    size_of_code: u32,
    size_of_initialized_data: u32,
    size_of_uninitialized_data: u32,
    address_of_entry_point: u32,
    base_of_code: u32,
    base_of_data: u32,
    image_base: u32,
    section_alignment: u32,
    file_alignment: u32,
    major_operating_system_version: u16,
    minor_operating_system_version: u16,
    major_image_version: u16,
    minor_image_version: u16,
    major_subsystem_version: u16,
    minor_subsystem_version: u16,
    win32_version_value: u32,
    size_of_image: u32,
    size_of_headers: u32,
    check_sum: u32,
    subsystem: u16,
    dll_characteristics: u16,
    size_of_stack_reserve: u32,
    size_of_stack_commit: u32,
    size_of_heap_reserve: u32,
    size_of_heap_commit: u32,
    loader_flags: u32,
    number_of_rva_and_sizes: u32,
    data_directory: [ImageDataDirectory; 16],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct ImageDataDirectory {
    virtual_address: u32,
    size: u32,
}

fn pe_get_entry_point_offset(pe32_data: &[u8]) -> Option<u32> {
    let dos_header = ImageDosHeader::read_from_prefix(pe32_data).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)
    let nt_headers_offset = if dos_header.e_magic == IMAGE_DOS_SIGNATURE {
        // DOS image header is present, so read the PE header after the DOS image header.
        dos_header.e_lfanew as usize
    } else {
        // DOS image header is not present, so PE header is at the image base.
        0
    };

    let signature = u32::read_from_prefix(&pe32_data[nt_headers_offset..])
        .ok()?
        .0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)

    // Calculate the entry point relative to the start of the image.
    // AddressOfEntryPoint is common for PE32 & PE32+
    if signature as u16 == TE_IMAGE_HEADER_SIGNATURE {
        let te = TeImageHeader::read_from_prefix(&pe32_data[nt_headers_offset..])
            .ok()?
            .0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)
        Some(te.address_of_entry_point + size_of_val(&te) as u32 - te.stripped_size as u32)
    } else if signature == IMAGE_NT_SIGNATURE {
        let pe = ImageNtHeaders32::read_from_prefix(&pe32_data[nt_headers_offset..])
            .ok()?
            .0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)
        Some(pe.optional_header.address_of_entry_point)
    } else {
        None
    }
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct EFI_FIRMWARE_VOLUME_HEADER {
    zero_vector: [u8; 16],
    file_system_guid: Guid,
    fv_length: u64,
    signature: u32,
    attributes: u32,
    header_length: u16,
    checksum: u16,
    ext_header_offset: u16,
    reserved: u8,
    revision: u8,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct EFI_FFS_FILE_HEADER {
    name: Guid,
    integrity_check: u16,
    typ: u8,
    attributes: u8,
    size: [u8; 3],
    state: u8,
}

const EFI_FV_FILETYPE_SECURITY_CORE: u8 = 3;

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct EFI_COMMON_SECTION_HEADER {
    size: [u8; 3],
    typ: u8,
}

const EFI_SECTION_PE32: u8 = 0x10;

/// Get the SEC entry point offset from the firmware base.
fn get_sec_entry_point_offset(image: &[u8]) -> Option<u64> {
    // Skip to SEC volume start.
    let mut image_offset = SEC_FIRMWARE_VOLUME_OFFSET;

    // Expect a firmware volume header for SEC volume.
    let fvh = EFI_FIRMWARE_VOLUME_HEADER::read_from_prefix(&image[image_offset as usize..])
        .ok()?
        .0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)
    if fvh.signature != EFI_FVH_SIGNATURE {
        return None;
    }

    // Skip past firmware volume header to beginning of firmware volume.
    image_offset += fvh.header_length as u64;

    // Find the first SEC CORE file type.
    let mut sec_core_file_header = None;
    let mut volume_offset = 0;
    while volume_offset < fvh.fv_length {
        let new_volume_offset = (volume_offset + 7) & !7;
        if new_volume_offset > volume_offset {
            image_offset += new_volume_offset - volume_offset;
            volume_offset = new_volume_offset;
        }
        let fh = EFI_FFS_FILE_HEADER::read_from_prefix(&image[image_offset as usize..])
            .ok()?
            .0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)
        if fh.typ == EFI_FV_FILETYPE_SECURITY_CORE {
            sec_core_file_header = Some(fh);
            break;
        }

        image_offset += expand_3byte_integer(fh.size);
        volume_offset += expand_3byte_integer(fh.size);
    }

    // There should always be a Security Core file.
    let sec_core_file_header = sec_core_file_header?;
    let sec_core_file_size = expand_3byte_integer(sec_core_file_header.size);

    // Move past the firmware file header.
    image_offset += size_of::<EFI_FFS_FILE_HEADER>() as u64;
    volume_offset += size_of::<EFI_FFS_FILE_HEADER>() as u64;

    // Loop through the firmware file sections looking for PE section.
    let mut file_offset = volume_offset;
    while file_offset < sec_core_file_size {
        //
        // Section headers are 8 byte aligned with respect to the beginning of the file stream.
        //
        let new_file_offset = (file_offset + 3) & !3;
        if new_file_offset > file_offset {
            image_offset += new_file_offset - file_offset;
            file_offset += new_file_offset - file_offset;
        }

        let sh = EFI_COMMON_SECTION_HEADER::read_from_prefix(&image[image_offset as usize..])
            .ok()?
            .0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)
        if sh.typ == EFI_SECTION_PE32 {
            let pe_offset = pe_get_entry_point_offset(
                &image[image_offset as usize + size_of::<EFI_COMMON_SECTION_HEADER>()..],
            )?;
            image_offset += size_of::<EFI_COMMON_SECTION_HEADER>() as u64 + pe_offset as u64;
            break;
        }
        image_offset += expand_3byte_integer(sh.size);
        file_offset += expand_3byte_integer(sh.size);
    }

    Some(image_offset)
}

/// Definitions shared by UEFI and the loader when loaded with parameters passed in IGVM format.
mod igvm {
    use zerocopy::FromBytes;

    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    /// The structure used to tell UEFI where the IGVM loaded parameters are.
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub struct UEFI_IGVM_PARAMETER_INFO {
        pub parameter_page_count: u32,
        pub cpuid_pages_offset: u32,
        pub vp_context_page_number: u64,
        pub loader_block_offset: u32,
        pub command_line_offset: u32,
        pub command_line_page_count: u32,
        pub memory_map_offset: u32,
        pub memory_map_page_count: u32,
        pub madt_offset: u32,
        pub madt_page_count: u32,
        pub srat_offset: u32,
        pub srat_page_count: u32,
        pub maximum_processor_count: u32,
        pub uefi_memory_map_offset: u32,
        pub uefi_memory_map_page_count: u32,
    }

    pub const UEFI_IGVM_LOADER_BLOCK_NUMBER_OF_PROCESSORS_FIELD_OFFSET: usize = 0;
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Firmware size invalid")]
    InvalidImageSize,
    #[error("Unable to find SEC volume entry point")]
    NoSecEntryPoint,
    #[error("Invalid shared gpa boundary")]
    InvalidSharedGpaBoundary,
    #[error("Invalid config type")]
    InvalidConfigType(String),
    #[error("Importer error")]
    Importer(#[source] anyhow::Error),
    #[error("PageTableBuilder: {0}")]
    PageTableBuilder(#[from] page_table::Error),
}

#[derive(Debug)]
pub enum ConfigType {
    ConfigBlob(config::Blob),
    Igvm,
    None,
}

#[derive(Debug)]
pub struct LoadInfo {
    /// The GPA the firmware was loaded at.
    pub firmware_base: u64,
    /// The size of the firmware image loaded, in bytes.
    pub firmware_size: u64,
    /// The total size used by the loader starting at the firmware_base,
    /// including the firmware image and misc data, in bytes.
    pub total_size: u64,
}

pub mod x86_64 {
    use super::ConfigType;
    use super::Error;
    use super::LoadInfo;
    use crate::common::DEFAULT_GDT_SIZE;
    use crate::common::import_default_gdt;
    use crate::cpuid::HV_PSP_CPUID_PAGE;
    use crate::importer::BootPageAcceptance;
    use crate::importer::IgvmParameterType;
    use crate::importer::ImageLoad;
    use crate::importer::IsolationType;
    use crate::importer::StartupMemoryType;
    use crate::importer::X86Register;
    use crate::uefi::SEC_FIRMWARE_VOLUME_OFFSET;
    use crate::uefi::get_sec_entry_point_offset;
    use hvdef::HV_PAGE_SIZE;
    use page_table::IdentityMapSize;
    use page_table::x64::IdentityMapBuilder;
    use page_table::x64::PAGE_TABLE_MAX_BYTES;
    use page_table::x64::PAGE_TABLE_MAX_COUNT;
    use page_table::x64::PageTable;
    use page_table::x64::align_up_to_page_size;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

    pub const IMAGE_SIZE: u64 = 0x00600000; // 6 MB. See MsvmPkg\MsvmPkgX64.fdf
    const IMAGE_GPA_BASE: u64 = 0x100000; // 1MB
    const PAGE_TABLE_GPA_BASE: u64 = IMAGE_GPA_BASE + IMAGE_SIZE; // 7MB - 0x700000
    const PAGE_TABLE_SIZE: u64 = HV_PAGE_SIZE * 6;
    const GDT_GPA_BASE: u64 = PAGE_TABLE_GPA_BASE + PAGE_TABLE_SIZE; // 0x707000
    const MISC_PAGES_GPA_BASE: u64 = GDT_GPA_BASE + DEFAULT_GDT_SIZE; // 0x707000
    const MISC_PAGES_SIZE: u64 = HV_PAGE_SIZE * 2;
    pub const CONFIG_BLOB_GPA_BASE: u64 = MISC_PAGES_GPA_BASE + MISC_PAGES_SIZE; // 0x709000

    /// Load a UEFI image with the provided config type.
    pub fn load(
        importer: &mut dyn ImageLoad<X86Register>,
        image: &[u8],
        config: ConfigType,
    ) -> Result<LoadInfo, Error> {
        if image.len() != IMAGE_SIZE as usize {
            return Err(Error::InvalidImageSize);
        }

        let sec_entry_point = get_sec_entry_point_offset(image).ok_or(Error::NoSecEntryPoint)?;

        let isolation = importer.isolation_config();

        // Build the page tables. This depends on if we have a paravisor present or not:
        //      - If this is an SNP VM with no paravisor, then build a set of page tables
        //        to map the bottom 4GB of memory with shared visibility.
        //      - Otherwise, build the standard UEFI page tables. Bottom 4GB of address space,
        //        identity mapped with 2 MB pages.
        let mut page_table_work_buffer: Vec<PageTable> =
            vec![PageTable::new_zeroed(); PAGE_TABLE_MAX_COUNT];
        let mut page_tables: Vec<u8> = vec![0; PAGE_TABLE_MAX_BYTES];
        let page_table_builder = IdentityMapBuilder::new(
            PAGE_TABLE_GPA_BASE,
            IdentityMapSize::Size4Gb,
            page_table_work_buffer.as_mut_slice(),
            page_tables.as_mut_slice(),
        )?;
        let mut shared_vis_page_table_work_buffer: Vec<PageTable> = Vec::new();
        let mut shared_vis_page_tables: Vec<u8> = Vec::new();
        let (page_tables, shared_vis_page_tables) =
            if isolation.isolation_type == IsolationType::Snp && !isolation.paravisor_present {
                if let ConfigType::ConfigBlob(_) = config {
                    return Err(Error::InvalidConfigType(
                        "Enlightened UEFI must use IGVM parameters".into(),
                    ));
                }

                let shared_vis_page_table_gpa = CONFIG_BLOB_GPA_BASE + HV_PAGE_SIZE;
                let shared_gpa_boundary_bits = isolation
                    .shared_gpa_boundary_bits
                    .ok_or(Error::InvalidSharedGpaBoundary)?;
                let shared_gpa_boundary = 1 << shared_gpa_boundary_bits;

                shared_vis_page_table_work_buffer
                    .resize(PAGE_TABLE_MAX_COUNT, PageTable::new_zeroed());
                shared_vis_page_tables.resize(PAGE_TABLE_MAX_BYTES, 0);
                let shared_vis_builder = IdentityMapBuilder::new(
                    shared_vis_page_table_gpa,
                    IdentityMapSize::Size4Gb,
                    shared_vis_page_table_work_buffer.as_mut_slice(),
                    shared_vis_page_tables.as_mut_slice(),
                )?
                .with_address_bias(shared_gpa_boundary);

                // The extra page tables are placed after the first config blob
                // page.  They will be accounted for when the IGVM parameters are
                // built.
                let shared_vis_page_tables = shared_vis_builder.build();

                let page_tables = page_table_builder
                    .with_pml4e_link((shared_vis_page_table_gpa, shared_gpa_boundary))
                    .build();

                (page_tables, Some(shared_vis_page_tables))
            } else {
                let page_tables = page_table_builder.build();
                (page_tables, None)
            };

        // Size must match expected compiled constant
        assert_eq!(page_tables.len(), PAGE_TABLE_SIZE as usize);

        // Import image, page tables, GDT entries.
        let image_page_count = image.len() as u64 / HV_PAGE_SIZE;
        importer
            .import_pages(
                IMAGE_GPA_BASE / HV_PAGE_SIZE,
                image_page_count,
                "uefi-image",
                BootPageAcceptance::Exclusive,
                image,
            )
            .map_err(Error::Importer)?;

        let mut total_page_count = IMAGE_GPA_BASE / HV_PAGE_SIZE + image_page_count;

        importer
            .import_pages(
                PAGE_TABLE_GPA_BASE / HV_PAGE_SIZE,
                PAGE_TABLE_SIZE / HV_PAGE_SIZE,
                "uefi-page-tables",
                BootPageAcceptance::Exclusive,
                page_tables,
            )
            .map_err(Error::Importer)?;

        total_page_count += PAGE_TABLE_SIZE / HV_PAGE_SIZE;

        // The default GDT is used with a page count of one.
        assert_eq!(DEFAULT_GDT_SIZE, HV_PAGE_SIZE);
        import_default_gdt(importer, GDT_GPA_BASE / HV_PAGE_SIZE).map_err(Error::Importer)?;
        total_page_count += DEFAULT_GDT_SIZE / HV_PAGE_SIZE;

        // Reserve free pages. Currently these are only used by UEFI PEI for making hypercalls.
        importer
            .import_pages(
                MISC_PAGES_GPA_BASE / HV_PAGE_SIZE,
                MISC_PAGES_SIZE / HV_PAGE_SIZE,
                "uefi-misc-pages",
                BootPageAcceptance::Exclusive,
                &[],
            )
            .map_err(Error::Importer)?;

        total_page_count += MISC_PAGES_SIZE / HV_PAGE_SIZE;

        // Import the config blobg, if set. Some callers may not load UEFI
        // configuration at this time, such as if running with a paravisor.
        match config {
            ConfigType::Igvm => {
                total_page_count += set_igvm_parameters(
                    importer,
                    CONFIG_BLOB_GPA_BASE / HV_PAGE_SIZE,
                    match isolation.isolation_type {
                        IsolationType::Snp => shared_vis_page_tables
                            .as_ref()
                            .expect("should be shared vis page tables"),
                        _ => &[],
                    },
                )?
            }
            ConfigType::ConfigBlob(config) => {
                let data = config.complete();
                assert!(!data.is_empty());
                let config_blob_page_count = (data.len() as u64).div_ceil(HV_PAGE_SIZE);
                importer
                    .import_pages(
                        CONFIG_BLOB_GPA_BASE / HV_PAGE_SIZE,
                        config_blob_page_count,
                        "uefi-config-blob",
                        BootPageAcceptance::Exclusive,
                        &data,
                    )
                    .map_err(Error::Importer)?;

                total_page_count += config_blob_page_count;
            }
            ConfigType::None => {}
        }

        // UEFI expects that the memory from GPA 0 up until the end of the config
        // blob is present, at a minimum. Note that ImageGpaBase is not 0.
        importer
            .verify_startup_memory_available(0, total_page_count, StartupMemoryType::Ram)
            .map_err(Error::Importer)?;

        let mut import_reg = |register| {
            importer
                .import_vp_register(register)
                .map_err(Error::Importer)
        };

        // Set CR0
        import_reg(X86Register::Cr0(
            x86defs::X64_CR0_PG | x86defs::X64_CR0_NE | x86defs::X64_CR0_MP | x86defs::X64_CR0_PE,
        ))?;

        // Set CR3 to point to page table which starts right after the image.
        import_reg(X86Register::Cr3(PAGE_TABLE_GPA_BASE))?;

        // Set CR4
        import_reg(X86Register::Cr4(
            x86defs::X64_CR4_PAE
                | x86defs::X64_CR4_MCE
                | x86defs::X64_CR4_FXSR
                | x86defs::X64_CR4_XMMEXCPT,
        ))?;

        // Set EFER to LME, LMA, and NXE for 64 bit mode.
        import_reg(X86Register::Efer(
            x86defs::X64_EFER_LMA | x86defs::X64_EFER_LME | x86defs::X64_EFER_NXE,
        ))?;

        // Set PAT
        import_reg(X86Register::Pat(x86defs::X86X_MSR_DEFAULT_PAT))?;

        // Set register state to values SEC entry point expects.
        // RBP - start of BFV (sec FV)
        import_reg(X86Register::Rbp(
            IMAGE_GPA_BASE + SEC_FIRMWARE_VOLUME_OFFSET,
        ))?;

        // Set RIP to SEC entry point.
        import_reg(X86Register::Rip(IMAGE_GPA_BASE + sec_entry_point))?;

        // Set R8-R11 to the hypervisor isolation CPUID leaf values.
        let isolation_cpuid = isolation.get_cpuid();

        import_reg(X86Register::R8(isolation_cpuid.eax as u64))?;
        import_reg(X86Register::R9(isolation_cpuid.ebx as u64))?;
        import_reg(X86Register::R10(isolation_cpuid.ecx as u64))?;
        import_reg(X86Register::R11(isolation_cpuid.edx as u64))?;

        // Enable MTRRs, default MTRR is uncached, and set lowest 640KB as WB
        import_reg(X86Register::MtrrDefType(0xc00))?;
        import_reg(X86Register::MtrrFix64k00000(0x0606060606060606))?;
        import_reg(X86Register::MtrrFix16k80000(0x0606060606060606))?;

        Ok(LoadInfo {
            firmware_base: IMAGE_GPA_BASE,
            firmware_size: image.len() as u64,
            total_size: total_page_count * HV_PAGE_SIZE,
        })
    }

    /// A simple page allocator that supports allocating pages counting up from a base page.
    struct PageAllocator {
        base: u32,
        total_count: u32,
    }

    impl PageAllocator {
        /// Create a `PageAllocator` starting at the given page `base`.
        fn new(base: u32) -> PageAllocator {
            PageAllocator {
                base,
                total_count: 0,
            }
        }

        /// Allocate `count` number of pages. Returns the base page number for the allocation.
        fn allocate(&mut self, count: u32) -> u32 {
            let allocation = self.base + self.total_count;
            self.total_count += count;

            allocation
        }

        /// Get the total number of pages allocated.
        fn total(&self) -> u32 {
            self.total_count
        }
    }

    /// Construct the UEFI parameter information in IGVM format. `config_area_base_page` specifies the GPA page number
    /// at the start of the config region. The number of pages used in the config region is returned.
    fn set_igvm_parameters(
        importer: &mut dyn ImageLoad<X86Register>,
        config_area_base_page: u64,
        shared_visibility_page_tables: &[u8],
    ) -> Result<u64, Error> {
        let mut parameter_info = super::igvm::UEFI_IGVM_PARAMETER_INFO::new_zeroed();

        // IGVM UEFI_IGVM_PARAMETER_INFO page offsets are relative to 1, as the first page is taken by the
        // UEFI_IGVM_PARAMETER_INFO structure. Allocate a page for the UEFI_IGVM_PARAMETER_INFO structure.
        let mut allocator = PageAllocator::new(0);
        allocator.allocate(1);

        // Set up the parameter info structure with offsets to each of the
        // additional parameters. Each table allocates a constant number of
        // pages.
        let table_page_count = 20;

        // The first structure is the loader block, which happens after the parameter info structure and shared
        // visibility page tables.
        let page_table_page_count =
            align_up_to_page_size(shared_visibility_page_tables.len() as u64) / HV_PAGE_SIZE;
        let page_table_offset = allocator.allocate(page_table_page_count as u32);
        parameter_info.loader_block_offset = allocator.allocate(1);

        let command_line_page_count = 1;
        parameter_info.command_line_offset = allocator.allocate(command_line_page_count);
        parameter_info.command_line_page_count = command_line_page_count;

        parameter_info.memory_map_offset = allocator.allocate(table_page_count);
        parameter_info.memory_map_page_count = table_page_count;

        parameter_info.madt_offset = allocator.allocate(table_page_count);
        parameter_info.madt_page_count = table_page_count;

        parameter_info.srat_offset = allocator.allocate(table_page_count);
        parameter_info.srat_page_count = table_page_count;

        // Reserve additional pre-accepted pages for UEFI to use to reconstruct
        // portions of the config blob.
        parameter_info.uefi_memory_map_offset = allocator.allocate(table_page_count);
        parameter_info.uefi_memory_map_page_count = table_page_count;

        // If this is an SNP image with no paravisor, then reserve additional pages as required.
        let isolation = importer.isolation_config();
        if isolation.isolation_type == IsolationType::Snp {
            // NOTE: Currently UEFI expects this parameter load style to have no paravisor. Disallow that here.
            if isolation.paravisor_present {
                return Err(Error::InvalidConfigType(
                    "IGVM ConfigType specified but paravisor is present.".into(),
                ));
            }

            // Supply the address of the parameter info block so it can be used
            // before PEI parses the config information.
            importer
                .import_vp_register(X86Register::R12(config_area_base_page * HV_PAGE_SIZE))
                .map_err(Error::Importer)?;

            // Reserve two pages to hold CPUID information. The first CPUID page
            // contains initialized data to query CPUID leaves. The second page
            // contains no data, as it will be populated by the host when the
            // image is loaded.
            parameter_info.cpuid_pages_offset = allocator.allocate(2);

            let cpuid_page = create_snp_cpuid_page();

            importer
                .import_pages(
                    config_area_base_page + parameter_info.cpuid_pages_offset as u64,
                    1,
                    "uefi-cpuid-page",
                    BootPageAcceptance::CpuidPage,
                    cpuid_page.as_bytes(),
                )
                .map_err(Error::Importer)?;

            importer
                .import_pages(
                    config_area_base_page + parameter_info.cpuid_pages_offset as u64 + 1,
                    1,
                    "uefi-cpuid-extended-page",
                    BootPageAcceptance::CpuidExtendedStatePage,
                    &[],
                )
                .map_err(Error::Importer)?;

            // Reserve a page to use to hold the VMSA.  This must be reported to
            // UEFI so that the page can be marked as a permanent firmware
            // allocation.
            //
            // Note that this page must not be counted within the size of the
            // config block, since it has different memory protection properties.
            // The first page following the config block is chosen for the
            // allocation.
            let vp_context_page_number = config_area_base_page + allocator.total() as u64;
            importer
                .set_vp_context_page(vp_context_page_number)
                .map_err(Error::Importer)?;

            parameter_info.vp_context_page_number = vp_context_page_number;
        } else {
            // If this is not an SNP image, then the VP context page does not
            // need to be reported to UEFI. Put in the TDX reset page value for
            // consistency with old code; this probably is unnecessary (or the
            // UEFI firmware should just be improved to not need this).
            parameter_info.vp_context_page_number = 0xfffff;
        }

        // Encode the total amount of pages used by all parameters.
        parameter_info.parameter_page_count = allocator.total();

        importer
            .import_pages(
                config_area_base_page,
                1,
                "uefi-config-base-page",
                BootPageAcceptance::Exclusive,
                parameter_info.as_bytes(),
            )
            .map_err(Error::Importer)?;

        importer
            .import_pages(
                config_area_base_page + parameter_info.uefi_memory_map_offset as u64,
                parameter_info.uefi_memory_map_page_count as u64,
                "uefi-memory-map-scratch",
                BootPageAcceptance::ExclusiveUnmeasured,
                &[],
            )
            .map_err(Error::Importer)?;

        let loader_block = importer
            .create_parameter_area(
                config_area_base_page + parameter_info.loader_block_offset as u64,
                1,
                "uefi-loader-block",
            )
            .map_err(Error::Importer)?;
        importer
            .import_parameter(
                loader_block,
                super::igvm::UEFI_IGVM_LOADER_BLOCK_NUMBER_OF_PROCESSORS_FIELD_OFFSET as u32,
                IgvmParameterType::VpCount,
            )
            .map_err(Error::Importer)?;

        let command_line = importer
            .create_parameter_area(
                config_area_base_page + parameter_info.command_line_offset as u64,
                parameter_info.command_line_page_count,
                "uefi-command-line",
            )
            .map_err(Error::Importer)?;
        importer
            .import_parameter(command_line, 0, IgvmParameterType::CommandLine)
            .map_err(Error::Importer)?;

        let memory_map = importer
            .create_parameter_area(
                config_area_base_page + parameter_info.memory_map_offset as u64,
                parameter_info.memory_map_page_count,
                "uefi-memory-map",
            )
            .map_err(Error::Importer)?;
        importer
            .import_parameter(memory_map, 0, IgvmParameterType::MemoryMap)
            .map_err(Error::Importer)?;

        let madt = importer
            .create_parameter_area(
                config_area_base_page + parameter_info.madt_offset as u64,
                parameter_info.madt_page_count,
                "uefi-madt",
            )
            .map_err(Error::Importer)?;
        importer
            .import_parameter(madt, 0, IgvmParameterType::Madt)
            .map_err(Error::Importer)?;

        let srat = importer
            .create_parameter_area(
                config_area_base_page + parameter_info.srat_offset as u64,
                parameter_info.srat_page_count,
                "uefi-srat",
            )
            .map_err(Error::Importer)?;
        importer
            .import_parameter(srat, 0, IgvmParameterType::Srat)
            .map_err(Error::Importer)?;

        if page_table_page_count != 0 {
            importer
                .import_pages(
                    config_area_base_page + page_table_offset as u64,
                    page_table_page_count,
                    "uefi-igvm-page-tables",
                    BootPageAcceptance::Exclusive,
                    shared_visibility_page_tables,
                )
                .map_err(Error::Importer)?;
        }

        Ok(allocator.total() as u64)
    }

    /// Create a hypervisor SNP CPUID page with the default values.
    fn create_snp_cpuid_page() -> HV_PSP_CPUID_PAGE {
        let mut cpuid_page = HV_PSP_CPUID_PAGE::default();

        for (i, required_leaf) in crate::cpuid::SNP_REQUIRED_CPUID_LEAF_LIST_UEFI
            .iter()
            .enumerate()
        {
            cpuid_page.cpuid_leaf_info[i].eax_in = required_leaf.eax;
            cpuid_page.cpuid_leaf_info[i].eax_out = required_leaf.ecx;
            cpuid_page.count += 1;
        }

        cpuid_page
    }
}

pub mod aarch64 {
    use super::ConfigType;
    use super::Error;
    use super::LoadInfo;
    use crate::importer::Aarch64Register;
    use crate::importer::BootPageAcceptance;
    use crate::importer::ImageLoad;
    use aarch64defs::Cpsr64;
    use hvdef::HV_PAGE_SIZE;

    use zerocopy::IntoBytes;

    pub const IMAGE_SIZE: u64 = 0x800000;
    pub const CONFIG_BLOB_GPA_BASE: u64 = 0x824000;

    /// Load a UEFI image with the provided config type.
    pub fn load(
        importer: &mut dyn ImageLoad<Aarch64Register>,
        image: &[u8],
        config: ConfigType,
    ) -> Result<LoadInfo, Error> {
        if image.len() != IMAGE_SIZE as usize {
            return Err(Error::InvalidImageSize);
        }

        const BYTES_2MB: u64 = 0x200000;

        let image_size = (image.len() as u64 + BYTES_2MB - 1) & !(BYTES_2MB - 1);
        importer
            .import_pages(
                0,
                image_size / HV_PAGE_SIZE,
                "uefi-image",
                BootPageAcceptance::Exclusive,
                image,
            )
            .map_err(Error::Importer)?;

        // The stack.
        let stack_offset = image_size;
        let stack_size = 32 * HV_PAGE_SIZE;
        let stack_end = stack_offset + stack_size;
        importer
            .import_pages(
                stack_offset / HV_PAGE_SIZE,
                stack_size / HV_PAGE_SIZE,
                "uefi-stack",
                BootPageAcceptance::Exclusive,
                &[],
            )
            .map_err(Error::Importer)?;

        // The page tables.
        let page_table_offset = stack_end;
        let page_tables = page_tables(page_table_offset, 1 << 30 /* TODO */);
        importer
            .import_pages(
                page_table_offset / HV_PAGE_SIZE,
                page_tables.as_bytes().len() as u64 / HV_PAGE_SIZE,
                "uefi-page-tables",
                BootPageAcceptance::Exclusive,
                page_tables.as_bytes(),
            )
            .map_err(Error::Importer)?;

        let blob_offset = CONFIG_BLOB_GPA_BASE;

        // The config blob.
        let blob_size = match config {
            ConfigType::ConfigBlob(blob) => {
                let blob = blob.complete();
                let blob_size = (blob.len() as u64 + HV_PAGE_SIZE - 1) & !(HV_PAGE_SIZE - 1);
                importer
                    .import_pages(
                        blob_offset / HV_PAGE_SIZE,
                        blob_size / HV_PAGE_SIZE,
                        "uefi-config-blob",
                        BootPageAcceptance::Exclusive,
                        &blob,
                    )
                    .map_err(Error::Importer)?;

                blob_size
            }
            ConfigType::None => 0,
            ConfigType::Igvm => {
                return Err(Error::InvalidConfigType("igvm not supported".to_owned()));
            }
        };

        let total_size = blob_offset + blob_size;

        let mut import_reg = |reg| importer.import_vp_register(reg).map_err(Error::Importer);

        import_reg(Aarch64Register::Cpsr(
            Cpsr64::new().with_sp(true).with_el(1).into(),
        ))?;
        import_reg(Aarch64Register::X0(0x1000))?;
        import_reg(Aarch64Register::Pc(0x1000))?;
        import_reg(Aarch64Register::X1(stack_end))?;

        import_reg(Aarch64Register::Ttbr0El1(page_table_offset))?;

        // Memory attribute indirection register.
        const ARM64_MAIR_CACHE_WBWA: u64 = 0xff;
        const ARM64_MAIR_CACHE_NC: u64 = 0x00;
        const ARM64_MAIR_CACHE_WTNA: u64 = 0xaa;
        const ARM64_MAIR_CACHE_WC: u64 = 0x44;

        import_reg(Aarch64Register::MairEl1(
            ARM64_MAIR_CACHE_WBWA
                | (ARM64_MAIR_CACHE_NC << 8)
                | (ARM64_MAIR_CACHE_WTNA << 16)
                | (ARM64_MAIR_CACHE_WC << 24)
                | (ARM64_MAIR_CACHE_WBWA << 32)
                | (ARM64_MAIR_CACHE_NC << 40)
                | (ARM64_MAIR_CACHE_WTNA << 48)
                | (ARM64_MAIR_CACHE_WC << 56),
        ))?;

        // System control register.
        const ARM64_SCTLR_M: u64 = 0x00000001;
        const ARM64_SCTLR_C: u64 = 0x00000004;
        const ARM64_SCTLR_RES1_11: u64 = 0x00000800;
        const ARM64_SCTLR_I: u64 = 0x00001000;
        const ARM64_SCTLR_RES1_20: u64 = 0x00100000;
        const ARM64_SCTLR_RES1_22: u64 = 0x00400000;
        const ARM64_SCTLR_RES1_23: u64 = 0x00800000;
        const ARM64_SCTLR_RES1_28: u64 = 0x10000000;
        const ARM64_SCTLR_RES1_29: u64 = 0x20000000;

        import_reg(Aarch64Register::SctlrEl1(
            ARM64_SCTLR_M
                | ARM64_SCTLR_C
                | ARM64_SCTLR_I
                | ARM64_SCTLR_RES1_11
                | ARM64_SCTLR_RES1_20
                | ARM64_SCTLR_RES1_22
                | ARM64_SCTLR_RES1_23
                | ARM64_SCTLR_RES1_28
                | ARM64_SCTLR_RES1_29,
        ))?;

        // Translation control register.

        const ARM64_TCR_IRGN0_WBWA: u64 = 0x0000000000000100;
        const ARM64_TCR_ORGN0_WBWA: u64 = 0x0000000000000400;
        const ARM64_TCR_SH0_INNER_SHARED: u64 = 0x0000000000003000;
        const ARM64_TCR_TG0_4K: u64 = 0x0000000000000000;
        const ARM64_TCR_EPD1: u64 = 0x0000000000800000;
        const ARM64_TCR_T0SZ_SHIFT: u32 = 0;
        const ARM64_TCR_T1SZ_SHIFT: u32 = 16;

        import_reg(Aarch64Register::TcrEl1(
            ARM64_TCR_EPD1
                | ARM64_TCR_TG0_4K
                | ARM64_TCR_SH0_INNER_SHARED
                | ARM64_TCR_ORGN0_WBWA
                | ARM64_TCR_IRGN0_WBWA
                | (16 << ARM64_TCR_T0SZ_SHIFT)
                | (16 << ARM64_TCR_T1SZ_SHIFT),
        ))?;

        Ok(LoadInfo {
            firmware_base: 0,
            firmware_size: image.len() as u64,
            total_size,
        })
    }

    const PTE_VALID: u64 = 1 << 0;
    const PTE_NOT_LARGE: u64 = 1 << 1;
    const PTE_MAIR_WB: u64 = 0 << 2;
    const PTE_MAIR_UC: u64 = 1 << 2;
    const PTE_SHARABILITY_INNER: u64 = 3 << 8;
    const PTE_ACCESSED: u64 = 1 << 10;
    const PTE_USER_NX: u64 = 1 << 54;

    fn large_leaf_entry(normal: bool, address: u64) -> u64 {
        address
            | PTE_VALID
            | PTE_ACCESSED
            | PTE_SHARABILITY_INNER
            | PTE_USER_NX
            | if normal { PTE_MAIR_WB } else { PTE_MAIR_UC }
    }

    fn non_leaf_entry(address: u64) -> u64 {
        address | PTE_VALID | PTE_NOT_LARGE
    }

    fn leaf_entry(normal: bool, address: u64) -> u64 {
        address
            | PTE_VALID
            | PTE_ACCESSED
            | PTE_NOT_LARGE
            | PTE_SHARABILITY_INNER
            | PTE_USER_NX
            | if normal { PTE_MAIR_WB } else { PTE_MAIR_UC }
    }

    fn table_index(va: u64, level: u32) -> usize {
        let index = va >> (9 * (3 - level) + 12);
        let index = index & ((1 << 9) - 1);
        index as usize
    }

    fn page_tables(address: u64, end_of_ram: u64) -> Vec<[u64; 512]> {
        const PT_SIZE: u64 = 4096;
        const VA_4GB: u64 = 1 << 32;
        const VA_1GB: u64 = 1 << 30;
        const VA_2MB: u64 = 2 << 20;
        const VA_4KB: u64 = 4 << 10;

        let mut buffer = vec![[0u64; PT_SIZE as usize / 8]; 4];
        let [level0, level1, level2, level3] = buffer.as_mut_slice() else {
            unreachable!()
        };

        // Allocate temporary buffer to hold page tables. We need 4 page tables:
        // - PML4 table (level 0 table in ARM terminology).
        // - PDP table (level 1 table).
        // - PD table (level 2 table) to map the 1 GB region that contains the
        //   split between normal and device memory.
        // - PT table (level 3 table) to map the 2 MB region that contains the
        //   split between normal and device memory.

        // Link level 1 translation table.
        level0[0] = non_leaf_entry(address + PT_SIZE);

        // Create an identity map for the address space from 0 to 4 GB.
        // The range [0, 4GB - MMIO Space Size) is mapped as normal memory, the
        // range [4 GB - MMIO Space Size, 4 GB) is mapped as device memory.

        let mut normal = true;
        let mut va = 0;
        let mut end_va = end_of_ram;
        while va < VA_4GB {
            //
            // Switch to device memory if we are are within the MMIO space.
            //
            if normal && va == end_va {
                normal = false;
                end_va = VA_4GB;
                continue;
            }

            // Try to use a 1 GB page (level 1 block entry) if possible.
            let level1_index = table_index(va, 1);
            if level1[level1_index] & PTE_VALID == 0
                && ((va & (VA_1GB - 1)) == 0)
                && (end_va - va >= VA_1GB)
            {
                level1[level1_index] = large_leaf_entry(normal, va);
                va += VA_1GB;
                continue;
            }

            //
            // Allocate and link level 2 translation table (PD) if it does not yet
            // exist.
            //
            if level1[level1_index] & PTE_VALID == 0 {
                level1[level1_index] = non_leaf_entry(address + PT_SIZE * 2);
            }

            //
            // Try to use a 2 MB page (level 2 block entry) if possible.
            //
            let level2_index = table_index(va, 2);
            if level2[level2_index] & PTE_VALID == 0
                && ((va & (VA_2MB - 1)) == 0)
                && (end_va - va >= VA_2MB)
            {
                level2[level2_index] = large_leaf_entry(normal, va);
                va += VA_2MB;
                continue;
            }

            //
            // Allocate and link level 1 translation table (PT) if it does not yet
            // exist.
            //
            if level2[level2_index] & PTE_VALID == 0 {
                level2[level2_index] = non_leaf_entry(address + PT_SIZE * 3);
            }

            let level3_index = table_index(va, 3);
            level3[level3_index] = leaf_entry(normal, va);
            va += VA_4KB;
        }

        buffer
    }
}
