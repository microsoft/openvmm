// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Paravisor specific loader definitions and implementation.

use crate::cpuid::HV_PSP_CPUID_PAGE;
use crate::importer::Aarch64Register;
use crate::importer::BootPageAcceptance;
use crate::importer::IgvmParameterType;
use crate::importer::ImageLoad;
use crate::importer::IsolationConfig;
use crate::importer::IsolationType;
use crate::importer::SegmentRegister;
use crate::importer::StartupMemoryType;
use crate::importer::TableRegister;
use crate::importer::X86Register;
use crate::linux::InitrdAddressType;
use crate::linux::InitrdConfig;
use crate::linux::InitrdInfo;
use crate::linux::KernelInfo;
use crate::linux::load_kernel_and_initrd_arm64;
use aarch64defs::Cpsr64;
use aarch64defs::IntermPhysAddrSize;
use aarch64defs::SctlrEl1;
use aarch64defs::TranslationBaseEl1;
use aarch64defs::TranslationControlEl1;
use aarch64defs::TranslationGranule0;
use aarch64defs::TranslationGranule1;
use hvdef::HV_PAGE_SIZE;
use hvdef::Vtl;
use igvm::registers::AArch64Register;
use loader_defs::paravisor::*;
use loader_defs::shim::ShimParamsRaw;
use memory_range::MemoryRange;
use page_table::aarch64::Arm64PageSize;
use page_table::aarch64::MemoryAttributeEl1;
use page_table::aarch64::MemoryAttributeIndirectionEl1;
use page_table::x64::MappedRange;
use page_table::x64::PAGE_TABLE_MAX_BYTES;
use page_table::x64::PAGE_TABLE_MAX_COUNT;
use page_table::x64::PageTable;
use page_table::x64::PageTableBuilder;
use page_table::x64::X64_LARGE_PAGE_SIZE;
use page_table::x64::align_up_to_large_page_size;
use page_table::x64::align_up_to_page_size;
use page_table::x64::calculate_pde_table_count;
use thiserror::Error;
use x86defs::GdtEntry;
use x86defs::SegmentSelector;
use x86defs::X64_BUSY_TSS_SEGMENT_ATTRIBUTES;
use x86defs::X64_DEFAULT_CODE_SEGMENT_ATTRIBUTES;
use x86defs::X64_DEFAULT_DATA_SEGMENT_ATTRIBUTES;
use x86defs::cpuid::CpuidFunction;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

#[derive(Debug)]
pub struct Vtl0Linux<'a> {
    pub command_line: &'a std::ffi::CString,
    pub load_info: crate::linux::LoadInfo,
}

#[derive(Debug)]
pub struct Vtl0Config<'a> {
    pub supports_pcat: bool,
    /// The load info and the VP context page.
    pub supports_uefi: Option<(crate::uefi::LoadInfo, Vec<u8>)>,
    pub supports_linux: Option<Vtl0Linux<'a>>,
}

// See HclDefs.h
pub const HCL_SECURE_VTL: Vtl = Vtl::Vtl2;

/// Size of the persisted region (2MB).
const PERSISTED_REGION_SIZE: u64 = 2 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum Error {
    #[error("memory is unaligned: {0}")]
    MemoryUnaligned(u64),
    #[error("command line too large: {0}")]
    CommandLineSize(usize),
    #[error("kernel load error")]
    Kernel(#[source] crate::linux::Error),
    #[error("shim load error")]
    Shim(#[source] crate::elf::Error),
    #[error("invalid initrd size: {0}")]
    InvalidInitrdSize(u64),
    #[error("memory used: {0} is greater than available")]
    NotEnoughMemory(u64),
    #[error("importer error")]
    Importer(#[from] anyhow::Error),
    #[error("PageTableBuilder: {0}")]
    PageTableBuilder(#[from] page_table::Error),
}

/// Kernel Command line type.
pub enum CommandLineType<'a> {
    /// The command line is a static string.
    Static(&'a str),
    /// The command line is dynamic and host appendable via the chosen node in
    /// device tree, with initial data specified by the provided CStr. An empty
    /// base_string may be provided to allow the host to specify the full kernel
    /// command line.
    HostAppendable(&'a str),
}

/// Load the underhill kernel on x64.
///
/// An optional initrd may be specified.
///
/// An optional `memory_page_base` may be specified. This will disable
/// relocation support for underhill.
pub fn load_openhcl_x64<F>(
    importer: &mut dyn ImageLoad<X86Register>,
    kernel_image: &mut F,
    shim: &mut F,
    sidecar: Option<&mut F>,
    command_line: CommandLineType<'_>,
    initrd: Option<&[u8]>,
    memory_page_base: Option<u64>,
    memory_page_count: u64,
    vtl0_config: Vtl0Config<'_>,
) -> Result<(), Error>
where
    F: std::io::Read + std::io::Seek,
{
    let IsolationConfig {
        isolation_type,
        paravisor_present,
        shared_gpa_boundary_bits,
    } = importer.isolation_config();

    // If no explicit memory base is specified, load with relocation support.
    let with_relocation = memory_page_base.is_none() && isolation_type == IsolationType::None;

    let memory_start_address = memory_page_base
        .map(|page_number| page_number * HV_PAGE_SIZE)
        .unwrap_or(PARAVISOR_DEFAULT_MEMORY_BASE_ADDRESS);

    let memory_size = memory_page_count * HV_PAGE_SIZE;

    // OpenHCL is laid out as the following:
    // --- High Memory, 2MB aligned ---
    // free space
    //
    // page tables
    // 16 pages reserved for bootshim heap
    // 8K bootshim logs
    // IGVM parameters
    // reserved vtl2 ranges
    // initrd
    // openhcl_boot
    // sidecar, if configured
    // - pad to next 2MB -
    // kernel
    // optional 2mb bounce buf for CVM
    // persisted state region
    // --- Low memory, 2MB aligned ---

    // Paravisor memory ranges must be 2MB (large page) aligned.
    if !memory_start_address.is_multiple_of(X64_LARGE_PAGE_SIZE) {
        return Err(Error::MemoryUnaligned(memory_start_address));
    }

    if !memory_size.is_multiple_of(X64_LARGE_PAGE_SIZE) {
        return Err(Error::MemoryUnaligned(memory_size));
    }

    // The whole memory range must be present and VTL2 protectable for the
    // underhill kernel to work.
    importer.verify_startup_memory_available(
        memory_start_address / HV_PAGE_SIZE,
        memory_page_count,
        if paravisor_present {
            StartupMemoryType::Vtl2ProtectableRam
        } else {
            StartupMemoryType::Ram
        },
    )?;

    let kernel_acceptance = match isolation_type {
        IsolationType::Snp | IsolationType::Tdx => BootPageAcceptance::Shared,
        _ => BootPageAcceptance::Exclusive,
    };

    let mut offset = memory_start_address;

    // Reserve the first 2MB for a potential persisted state region. The first
    // 4K page is always the persisted state header, and the bootshim may decide
    // to use the the remaining pages for the protobuf payload.
    let persisted_region_base = offset;
    let persisted_region_size = PERSISTED_REGION_SIZE;
    offset += persisted_region_size;

    // If hardware isolated, reserve a 2MB range for bounce buffering shared
    // pages. This is done first because we know the start address is 2MB
    // aligned, with the next consumers wanting 2MB aligned ranges. This is
    // reserved at load time in order to guarantee the pagetables have entries
    // for this identity mapping.
    //
    // Leave this as a gap, as there's no need to accept or describe this range
    // in the IGVM file.
    let bounce_buffer = if matches!(isolation_type, IsolationType::Snp | IsolationType::Tdx) {
        let bounce_buffer_gpa = offset;
        assert_eq!(bounce_buffer_gpa % X64_LARGE_PAGE_SIZE, 0);
        let range = MemoryRange::new(bounce_buffer_gpa..bounce_buffer_gpa + X64_LARGE_PAGE_SIZE);

        offset += range.len();
        Some(range)
    } else {
        None
    };

    tracing::trace!(offset, "loading the kernel");

    // The x86_64 uncompressed kernel we use doesn't show any difference
    // in the code sections upon flipping CONFIG_RELOCATABLE. In total,
    // there are 6 places where a difference is found: dates in the Linux
    // banner, GNU build ID, and metadata entries in the empty initrd image
    // (it always is embedded into the kernel). No sections with relocations
    // appear if CONFIG_RELOCATABLE is set.
    // Assume that at least the kernel entry contains PIC and no loader
    // assistance with the relocations records (if any) is required.
    let load_info = crate::elf::load_static_elf(
        importer,
        kernel_image,
        offset,
        0,
        true,
        kernel_acceptance,
        "underhill-kernel",
    )
    .map_err(|e| Error::Kernel(crate::linux::Error::ElfLoader(e)))?;
    tracing::trace!("Kernel loaded at {load_info:x?}");
    let crate::elf::LoadInfo {
        minimum_address_used: _min_addr,
        next_available_address: mut offset,
        entrypoint: kernel_entrypoint,
    } = load_info;

    assert_eq!(offset & (HV_PAGE_SIZE - 1), 0);

    // If an AP kernel was provided, load it next.
    let (sidecar_size, sidecar_entrypoint) = if let Some(sidecar) = sidecar {
        // Sidecar load addr must be 2MB aligned
        offset = align_up_to_large_page_size(offset);

        let load_info = crate::elf::load_static_elf(
            importer,
            sidecar,
            0,
            offset,
            false,
            BootPageAcceptance::Exclusive,
            "sidecar-kernel",
        )
        .map_err(|e| Error::Kernel(crate::linux::Error::ElfLoader(e)))?;

        (
            load_info.next_available_address - offset,
            load_info.entrypoint,
        )
    } else {
        (0, 0)
    };

    let sidecar_base = offset;
    offset += sidecar_size;

    let load_info = crate::elf::load_static_elf(
        importer,
        shim,
        0,
        offset,
        false,
        BootPageAcceptance::Exclusive,
        "underhill-boot-shim",
    )
    .map_err(Error::Shim)?;
    tracing::trace!("The boot shim loaded at {load_info:x?}");
    let crate::elf::LoadInfo {
        minimum_address_used: shim_base_addr,
        next_available_address: mut offset,
        entrypoint: shim_entry_address,
    } = load_info;

    // Optionally import initrd if specified.
    let ramdisk = if let Some(initrd) = initrd {
        let initrd_base = offset;
        let initrd_size = align_up_to_page_size(initrd.len() as u64);

        importer.import_pages(
            initrd_base / HV_PAGE_SIZE,
            initrd_size / HV_PAGE_SIZE,
            "underhill-initrd",
            kernel_acceptance,
            initrd,
        )?;

        offset += initrd_size;
        Some((initrd_base, initrd.len() as u64))
    } else {
        None
    };

    let gdt_base_address = offset;
    let gdt_size = HV_PAGE_SIZE;
    offset += gdt_size;

    let boot_params_base = offset;
    let boot_params_size = HV_PAGE_SIZE;

    offset += boot_params_size;

    let cmdline_base = offset;
    let (cmdline, policy) = match command_line {
        CommandLineType::Static(val) => (val, CommandLinePolicy::STATIC),
        CommandLineType::HostAppendable(val) => (val, CommandLinePolicy::APPEND_CHOSEN),
    };

    if cmdline.len() > COMMAND_LINE_SIZE {
        return Err(Error::CommandLineSize(cmdline.len()));
    }

    let mut static_command_line = [0; COMMAND_LINE_SIZE];
    static_command_line[..cmdline.len()].copy_from_slice(cmdline.as_bytes());
    let paravisor_command_line = ParavisorCommandLine {
        policy,
        static_command_line_len: cmdline.len() as u16,
        static_command_line,
    };

    importer.import_pages(
        cmdline_base / HV_PAGE_SIZE,
        1,
        "underhill-command-line",
        BootPageAcceptance::Exclusive,
        paravisor_command_line.as_bytes(),
    )?;

    offset += HV_PAGE_SIZE;

    // Reserve space for the VTL2 reserved region.
    let reserved_region_size = PARAVISOR_RESERVED_VTL2_PAGE_COUNT_MAX * HV_PAGE_SIZE;
    let reserved_region_start = offset;
    offset += reserved_region_size;

    tracing::debug!(reserved_region_start);

    let parameter_region_size = PARAVISOR_VTL2_CONFIG_REGION_PAGE_COUNT_MAX * HV_PAGE_SIZE;
    let parameter_region_start = offset;
    offset += parameter_region_size;

    tracing::debug!(parameter_region_start);

    // Reserve 8K for the bootshim log buffer. Import these pages so they are
    // available early without extra acceptance calls.
    let bootshim_log_size = HV_PAGE_SIZE * 2;
    let bootshim_log_start = offset;
    offset += bootshim_log_size;

    importer.import_pages(
        bootshim_log_start / HV_PAGE_SIZE,
        bootshim_log_size / HV_PAGE_SIZE,
        "ohcl-boot-shim-log-buffer",
        BootPageAcceptance::Exclusive,
        &[],
    )?;

    // Reserve 16 pages for a bootshim heap. This is only used to parse the
    // protobuf payload from the previous instance in a servicing boot.
    //
    // Import these pages as it greatly simplifies the early startup code in the
    // bootshim for isolated guests. This allows the bootshim to use these pages
    // early on without extra acceptance calls.
    let heap_start = offset;
    let heap_size = 16 * HV_PAGE_SIZE;
    importer.import_pages(
        heap_start / HV_PAGE_SIZE,
        heap_size / HV_PAGE_SIZE,
        "ohcl-boot-shim-heap",
        BootPageAcceptance::Exclusive,
        &[],
    )?;
    offset += heap_size;

    // The end of memory used by the loader, excluding pagetables.
    let end_of_underhill_mem = offset;

    // Page tables live at the end of VTL2 ram used by the bootshim.
    //
    // Size the available page table memory as 5 pages + 2 * 1GB of memory. This
    // allows underhill to be mapped across a 512 GB boundary when using more
    // than 1 GB, as the PDPTE will span 2 PML4E entries. Each GB of memory
    // mapped requires 1 page for 2MB pages. Give 2 extra base pages and 1
    // additional page per GB of mapped memory to allow the page table
    // relocation code to be simpler, and not need to reclaim free pages from
    // tables that have no valid entries.
    //
    // FUTURE: It would be better to change it so the shim only needs to map
    //         itself, kernel, initrd and IGVM parameters. This requires
    //         changing how the e820 map is constructed for the kernel along
    //         with changing the contract on where the IGVM parameters live
    //         within VTL2's memory.
    let local_map = match isolation_type {
        IsolationType::Snp | IsolationType::Tdx => {
            Some((PARAVISOR_LOCAL_MAP_VA, PARAVISOR_LOCAL_MAP_SIZE))
        }
        _ => None,
    };

    let page_table_base_page_count = 5;
    let page_table_dynamic_page_count = {
        // Double the count to allow for simpler reconstruction.
        calculate_pde_table_count(memory_start_address, memory_size) * 2
            + local_map.map_or(0, |v| calculate_pde_table_count(v.0, v.1))
    };
    let page_table_isolation_page_count = match isolation_type {
        IsolationType::Tdx => {
            // TDX requires up to an extra 3 pages to map the reset vector as a
            // 4K page.
            3
        }
        _ => 0,
    };
    let page_table_page_count = page_table_base_page_count
        + page_table_dynamic_page_count
        + page_table_isolation_page_count;
    let page_table_region_size = HV_PAGE_SIZE * page_table_page_count;
    let page_table_region_start = offset;
    offset += page_table_region_size;

    tracing::debug!(page_table_region_start, page_table_region_size);

    // Construct the memory ranges that will be identity mapped
    let mut ranges: Vec<MappedRange> = Vec::new();

    ranges.push(MappedRange::new(
        memory_start_address,
        memory_start_address + memory_size,
    ));

    if let Some((local_map_start, size)) = local_map {
        ranges.push(MappedRange::new(local_map_start, local_map_start + size));
    }

    if isolation_type == IsolationType::Tdx {
        const RESET_VECTOR_ADDR: u64 = 0xffff_f000;
        ranges.push(MappedRange::new(
            RESET_VECTOR_ADDR,
            RESET_VECTOR_ADDR + page_table::x64::X64_PAGE_SIZE,
        ));
    }

    ranges.sort_by_key(|r| r.start());

    // Initialize the page table builder, and build the page table
    let mut page_table_work_buffer: Vec<PageTable> =
        vec![PageTable::new_zeroed(); PAGE_TABLE_MAX_COUNT];
    let mut page_table: Vec<u8> = vec![0; PAGE_TABLE_MAX_BYTES];
    let mut page_table_builder = PageTableBuilder::new(
        page_table_region_start,
        page_table_work_buffer.as_mut_slice(),
        page_table.as_mut_slice(),
        ranges.as_slice(),
    )?;

    if isolation_type == IsolationType::Snp {
        page_table_builder = page_table_builder.with_confidential_bit(51);
    }

    let page_table = page_table_builder.build()?;

    assert!((page_table.len() as u64).is_multiple_of(HV_PAGE_SIZE));
    let page_table_page_base = page_table_region_start / HV_PAGE_SIZE;
    assert!(page_table.len() as u64 <= page_table_region_size);
    let offset = offset;

    if with_relocation {
        // Indicate relocation information. Don't include page table region.
        importer.relocation_region(
            memory_start_address,
            end_of_underhill_mem - memory_start_address,
            X64_LARGE_PAGE_SIZE,
            PARAVISOR_DEFAULT_MEMORY_BASE_ADDRESS,
            1 << 48,
            true,
            true,
            0, // BSP
        )?;

        // Tell the loader page table relocation information.
        importer.page_table_relocation(
            page_table_region_start,
            page_table_region_size / HV_PAGE_SIZE,
            page_table.len() as u64 / HV_PAGE_SIZE,
            0,
        )?;
    }

    // The memory used by the loader must be smaller than the memory available.
    if offset > memory_start_address + memory_size {
        return Err(Error::NotEnoughMemory(offset - memory_start_address));
    }

    let (initrd_base, initrd_size) = ramdisk.unwrap_or((0, 0));
    // Shim parameters for locations are relative to the base of where the shim is loaded.
    let calculate_shim_offset = |addr: u64| addr.wrapping_sub(shim_base_addr) as i64;
    let initrd_crc = crc32fast::hash(initrd.unwrap_or(&[]));
    let shim_params = ShimParamsRaw {
        kernel_entry_offset: calculate_shim_offset(kernel_entrypoint),
        cmdline_offset: calculate_shim_offset(cmdline_base),
        initrd_offset: calculate_shim_offset(initrd_base),
        initrd_size,
        initrd_crc,
        supported_isolation_type: match isolation_type {
            // To the shim, None and VBS isolation are the same. The shim
            // queries CPUID when running to determine if page acceptance needs
            // to be done.
            IsolationType::None | IsolationType::Vbs => {
                loader_defs::shim::SupportedIsolationType::VBS
            }
            IsolationType::Snp => loader_defs::shim::SupportedIsolationType::SNP,
            IsolationType::Tdx => loader_defs::shim::SupportedIsolationType::TDX,
        },
        memory_start_offset: calculate_shim_offset(memory_start_address),
        memory_size,
        parameter_region_offset: calculate_shim_offset(parameter_region_start),
        parameter_region_size,
        vtl2_reserved_region_offset: calculate_shim_offset(reserved_region_start),
        vtl2_reserved_region_size: reserved_region_size,
        sidecar_offset: calculate_shim_offset(sidecar_base),
        sidecar_size,
        sidecar_entry_offset: calculate_shim_offset(sidecar_entrypoint),
        used_start: calculate_shim_offset(memory_start_address),
        used_end: calculate_shim_offset(offset),
        bounce_buffer_start: bounce_buffer.map_or(0, |r| calculate_shim_offset(r.start())),
        bounce_buffer_size: bounce_buffer.map_or(0, |r| r.len()),
        log_buffer_start: calculate_shim_offset(bootshim_log_start),
        log_buffer_size: bootshim_log_size,
        heap_start_offset: calculate_shim_offset(heap_start),
        heap_size,
        persisted_state_region_offset: calculate_shim_offset(persisted_region_base),
        persisted_state_region_size: persisted_region_size,
    };

    tracing::debug!(boot_params_base, "shim gpa");

    importer
        .import_pages(
            boot_params_base / HV_PAGE_SIZE,
            boot_params_size / HV_PAGE_SIZE,
            "underhill-shim-params",
            BootPageAcceptance::Exclusive,
            shim_params.as_bytes(),
        )
        .map_err(Error::Importer)?;

    importer.import_pages(
        page_table_page_base,
        page_table_page_count,
        "underhill-page-tables",
        BootPageAcceptance::Exclusive,
        page_table,
    )?;

    // Set selectors and control registers
    // Setup two selectors and segment registers.
    // ds, es, fs, gs, ss are linearSelector
    // cs is linearCode64Selector

    // GDT is laid out as (counting by the small entries):
    //  0: null descriptor,
    //  1: null descriptor,
    //  2: linear code64 descriptor,
    //  3. linear descriptor for data
    //  4: here you can add more descriptors.

    let default_data_attributes: u16 = X64_DEFAULT_DATA_SEGMENT_ATTRIBUTES.into();
    let default_code64_attributes: u16 = X64_DEFAULT_CODE_SEGMENT_ATTRIBUTES.into();
    let gdt = [
        // A large null descriptor.
        GdtEntry::new_zeroed(),
        GdtEntry::new_zeroed(),
        // Code descriptor for the long mode.
        GdtEntry {
            limit_low: 0xffff,
            attr_low: default_code64_attributes as u8,
            attr_high: (default_code64_attributes >> 8) as u8,
            ..GdtEntry::new_zeroed()
        },
        // Data descriptor.
        GdtEntry {
            limit_low: 0xffff,
            attr_low: default_data_attributes as u8,
            attr_high: (default_data_attributes >> 8) as u8,
            ..GdtEntry::new_zeroed()
        },
    ];

    const LINEAR_CODE64_DESCRIPTOR_INDEX: usize = 2;
    const LINEAR_DATA_DESCRIPTOR_INDEX: usize = 3;
    const RPL: u8 = 0x00; // requested priviledge level: the highest

    let linear_code64_descriptor_selector =
        SegmentSelector::from_gdt_index(LINEAR_CODE64_DESCRIPTOR_INDEX as u16, RPL);
    let linear_data_descriptor_selector =
        SegmentSelector::from_gdt_index(LINEAR_DATA_DESCRIPTOR_INDEX as u16, RPL);

    importer.import_pages(
        gdt_base_address / HV_PAGE_SIZE,
        gdt_size / HV_PAGE_SIZE,
        "underhill-gdt",
        BootPageAcceptance::Exclusive,
        gdt.as_bytes(),
    )?;

    let mut import_reg = |register| {
        importer
            .import_vp_register(register)
            .map_err(Error::Importer)
    };

    // Import GDTR and selectors.
    import_reg(X86Register::Gdtr(TableRegister {
        base: gdt_base_address,
        limit: (size_of_val(&gdt) - 1) as u16,
    }))?;

    let ds = SegmentRegister {
        selector: linear_data_descriptor_selector.into_bits(),
        base: 0,
        limit: 0xffffffff,
        attributes: default_data_attributes,
    };
    import_reg(X86Register::Ds(ds))?;
    import_reg(X86Register::Es(ds))?;
    import_reg(X86Register::Fs(ds))?;
    import_reg(X86Register::Gs(ds))?;
    import_reg(X86Register::Ss(ds))?;

    let cs = SegmentRegister {
        selector: linear_code64_descriptor_selector.into_bits(),
        base: 0,
        limit: 0xffffffff,
        attributes: default_code64_attributes,
    };
    import_reg(X86Register::Cs(cs))?;

    // TODO: Workaround an OS repo bug where enabling a higher VTL zeros TR
    //       instead of setting it to the reset default state. Manually set it
    //       to the reset default state until the OS repo is fixed.
    //
    //       In the future, we should just not set this at all.
    import_reg(X86Register::Tr(SegmentRegister {
        selector: 0x0000,
        base: 0x00000000,
        limit: 0x0000FFFF,
        attributes: X64_BUSY_TSS_SEGMENT_ATTRIBUTES.into(),
    }))?;

    // Set system registers to state expected by the boot shim, 64 bit mode with
    // paging enabled.

    // Set CR0
    import_reg(X86Register::Cr0(
        x86defs::X64_CR0_PG | x86defs::X64_CR0_PE | x86defs::X64_CR0_NE,
    ))?;

    // Set CR3 to point to page table
    import_reg(X86Register::Cr3(page_table_region_start))?;

    // Set CR4
    import_reg(X86Register::Cr4(
        x86defs::X64_CR4_PAE | x86defs::X64_CR4_MCE | x86defs::X64_CR4_OSXSAVE,
    ))?;

    // Set EFER to LMA, LME, and NXE for 64 bit mode.
    import_reg(X86Register::Efer(
        x86defs::X64_EFER_LMA | x86defs::X64_EFER_LME | x86defs::X64_EFER_NXE,
    ))?;

    // Set PAT
    import_reg(X86Register::Pat(x86defs::X86X_MSR_DEFAULT_PAT))?;

    // Setup remaining registers
    // Set %rsi to relative location of boot_params_base
    let relative_boot_params_base = boot_params_base - shim_base_addr;
    import_reg(X86Register::Rsi(relative_boot_params_base))?;

    // Set %rip to the shim entry point.
    import_reg(X86Register::Rip(shim_entry_address))?;

    // Load parameter regions.
    let config_region_page_base = parameter_region_start / HV_PAGE_SIZE;

    // Slit
    let slit_page_base = config_region_page_base + PARAVISOR_CONFIG_SLIT_PAGE_INDEX;
    let slit_parameter_area = importer.create_parameter_area(
        slit_page_base,
        PARAVISOR_CONFIG_SLIT_SIZE_PAGES as u32,
        "underhill-slit",
    )?;
    importer.import_parameter(slit_parameter_area, 0, IgvmParameterType::Slit)?;

    // Pptt
    let pptt_page_base = config_region_page_base + PARAVISOR_CONFIG_PPTT_PAGE_INDEX;
    let pptt_parameter_area = importer.create_parameter_area(
        pptt_page_base,
        PARAVISOR_CONFIG_PPTT_SIZE_PAGES as u32,
        "underhill-pptt",
    )?;
    importer.import_parameter(pptt_parameter_area, 0, IgvmParameterType::Pptt)?;

    // device tree
    let dt_page_base = config_region_page_base + PARAVISOR_CONFIG_DEVICE_TREE_PAGE_INDEX;
    let dt_parameter_area = importer.create_parameter_area(
        dt_page_base,
        PARAVISOR_CONFIG_DEVICE_TREE_SIZE_PAGES as u32,
        "underhill-device-tree",
    )?;
    importer.import_parameter(dt_parameter_area, 0, IgvmParameterType::DeviceTree)?;

    if isolation_type == IsolationType::Snp {
        let reserved_region_page_base = reserved_region_start / HV_PAGE_SIZE;
        let secrets_page_base: u64 =
            reserved_region_page_base + PARAVISOR_RESERVED_VTL2_SNP_SECRETS_PAGE_INDEX;
        importer.import_pages(
            secrets_page_base,
            PARAVISOR_RESERVED_VTL2_SNP_SECRETS_SIZE_PAGES,
            "underhill-snp-secrets-page",
            BootPageAcceptance::SecretsPage,
            &[],
        )?;

        let cpuid_page = create_snp_cpuid_page();
        let cpuid_page_base =
            reserved_region_page_base + PARAVISOR_RESERVED_VTL2_SNP_CPUID_PAGE_INDEX;
        importer.import_pages(
            cpuid_page_base,
            1,
            "underhill-snp-cpuid-page",
            BootPageAcceptance::CpuidPage,
            cpuid_page.as_bytes(),
        )?;

        importer.import_pages(
            cpuid_page_base + 1,
            1,
            "underhill-snp-cpuid-extended-state-page",
            BootPageAcceptance::CpuidExtendedStatePage,
            &[],
        )?;

        let vmsa_page_base =
            reserved_region_page_base + PARAVISOR_RESERVED_VTL2_SNP_VMSA_PAGE_INDEX;
        importer.set_vp_context_page(vmsa_page_base)?;
    }

    // Load measured config.
    // The measured config is at page 0. Free pages start at page 1.
    let mut free_page = 1;
    let mut measured_config = ParavisorMeasuredVtl0Config {
        magic: ParavisorMeasuredVtl0Config::MAGIC,
        ..FromZeros::new_zeroed()
    };

    let Vtl0Config {
        supports_pcat,
        supports_uefi,
        supports_linux,
    } = vtl0_config;

    if supports_pcat {
        measured_config.supported_vtl0.set_pcat_supported(true);
    }

    if let Some((uefi, vp_context)) = &supports_uefi {
        measured_config.supported_vtl0.set_uefi_supported(true);
        let vp_context_page = free_page;
        free_page += 1;
        measured_config.uefi_info = UefiInfo {
            firmware: PageRegionDescriptor {
                base_page_number: uefi.firmware_base / HV_PAGE_SIZE,
                page_count: uefi.total_size / HV_PAGE_SIZE,
            },
            vtl0_vp_context: PageRegionDescriptor {
                base_page_number: vp_context_page,
                page_count: 1,
            },
        };

        // Deposit the UEFI vp context.
        importer.import_pages(
            vp_context_page,
            1,
            "openhcl-uefi-vp-context",
            BootPageAcceptance::Exclusive,
            vp_context,
        )?;
    }

    if let Some(linux) = supports_linux {
        measured_config
            .supported_vtl0
            .set_linux_direct_supported(true);

        let kernel_region = PageRegionDescriptor::new(
            linux.load_info.kernel.gpa / HV_PAGE_SIZE,
            align_up_to_page_size(linux.load_info.kernel.size) / HV_PAGE_SIZE,
        );

        let (initrd_region, initrd_size) = match linux.load_info.initrd {
            Some(info) => {
                if info.gpa % HV_PAGE_SIZE != 0 {
                    return Err(Error::MemoryUnaligned(info.gpa));
                }
                (
                    // initrd info is aligned up to the next page.
                    PageRegionDescriptor::new(
                        info.gpa / HV_PAGE_SIZE,
                        align_up_to_page_size(info.size) / HV_PAGE_SIZE,
                    ),
                    info.size,
                )
            }
            None => (PageRegionDescriptor::EMPTY, 0),
        };

        let command_line_page = free_page;
        // free_page += 1;

        // Import the command line as a C string.
        importer
            .import_pages(
                command_line_page,
                1,
                "underhill-vtl0-linux-command-line",
                BootPageAcceptance::Exclusive,
                linux.command_line.as_bytes_with_nul(),
            )
            .map_err(Error::Importer)?;
        let command_line = PageRegionDescriptor::new(command_line_page, 1);

        measured_config.linux_info = LinuxInfo {
            kernel_region,
            kernel_entrypoint: linux.load_info.kernel.entrypoint,
            initrd_region,
            initrd_size,
            command_line,
        };
    }

    importer
        .import_pages(
            PARAVISOR_VTL0_MEASURED_CONFIG_BASE_PAGE_X64,
            1,
            "underhill-measured-config",
            BootPageAcceptance::Exclusive,
            measured_config.as_bytes(),
        )
        .map_err(Error::Importer)?;

    let vtl2_measured_config = ParavisorMeasuredVtl2Config {
        magic: ParavisorMeasuredVtl2Config::MAGIC,
        vtom_offset_bit: shared_gpa_boundary_bits.unwrap_or(0),
        padding: [0; 7],
    };

    importer
        .import_pages(
            config_region_page_base + PARAVISOR_MEASURED_VTL2_CONFIG_PAGE_INDEX,
            PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES,
            "underhill-vtl2-measured-config",
            BootPageAcceptance::Exclusive,
            vtl2_measured_config.as_bytes(),
        )
        .map_err(Error::Importer)?;

    let imported_region_base =
        config_region_page_base + PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_PAGE_INDEX;

    importer.set_imported_regions_config_page(imported_region_base);
    Ok(())
}

/// Create a hypervisor SNP CPUID page with the default values.
fn create_snp_cpuid_page() -> HV_PSP_CPUID_PAGE {
    let mut cpuid_page = HV_PSP_CPUID_PAGE::default();

    // TODO SNP: The list used here is based earlier Microsoft projects.
    // 1. ExtendedStateEnumeration should be part of BootPageAcceptance::CpuidExtendedStatePage,
    // but it is unclear whether Linux supports a second page. The need for the second page is that
    // the entries in it are actually based on supported features on a specific host.
    // 2. ExtendedStateEnumeration should specify Xfem = 3
    for (i, required_leaf) in crate::cpuid::SNP_REQUIRED_CPUID_LEAF_LIST_PARAVISOR
        .iter()
        .enumerate()
    {
        let entry = &mut cpuid_page.cpuid_leaf_info[i];
        entry.eax_in = required_leaf.eax;
        entry.ecx_in = required_leaf.ecx;
        if required_leaf.eax == CpuidFunction::ExtendedStateEnumeration.0 {
            entry.xfem_in = 1;
        }
        cpuid_page.count += 1;
    }

    cpuid_page
}

/// Load the underhill kernel on arm64.
///
/// An optional initrd may be specified.
///
/// An optional `memory_page_base` may be specified. This will disable
/// relocation support for underhill.
pub fn load_openhcl_arm64<F>(
    importer: &mut dyn ImageLoad<Aarch64Register>,
    kernel_image: &mut F,
    shim: &mut F,
    command_line: CommandLineType<'_>,
    initrd: Option<&[u8]>,
    memory_page_base: Option<u64>,
    memory_page_count: u64,
    vtl0_config: Vtl0Config<'_>,
) -> Result<(), Error>
where
    F: std::io::Read + std::io::Seek,
{
    let Vtl0Config {
        supports_pcat,
        supports_uefi,
        supports_linux,
    } = vtl0_config;

    assert!(!supports_pcat);
    assert!(supports_uefi.is_some() || supports_linux.is_some());

    let paravisor_present = importer.isolation_config().paravisor_present;

    // If no explicit memory base is specified, load with relocation support.
    let with_relocation = memory_page_base.is_none();

    let memory_start_address = memory_page_base
        .map(|page_number| page_number * HV_PAGE_SIZE)
        .unwrap_or(PARAVISOR_DEFAULT_MEMORY_BASE_ADDRESS);

    let memory_size = memory_page_count * HV_PAGE_SIZE;

    // Paravisor memory ranges must be 2MB (large page) aligned.
    if !memory_start_address.is_multiple_of(u64::from(Arm64PageSize::Large)) {
        return Err(Error::MemoryUnaligned(memory_start_address));
    }

    if !memory_size.is_multiple_of(u64::from(Arm64PageSize::Large)) {
        return Err(Error::MemoryUnaligned(memory_size));
    }

    // The whole memory range must be present and VTL2 protectable for the
    // underhill kernel to work.
    importer.verify_startup_memory_available(
        memory_start_address / HV_PAGE_SIZE,
        memory_page_count,
        if paravisor_present {
            StartupMemoryType::Vtl2ProtectableRam
        } else {
            StartupMemoryType::Ram
        },
    )?;

    let mut next_addr = memory_start_address;

    // Reserve the first 2MB for a potential persisted state region. The first
    // 4K page is always the persisted state header, and the bootshim may decide
    // to use the the remaining pages for the protobuf payload.
    let persisted_region_base = next_addr;
    let persisted_region_size = PERSISTED_REGION_SIZE;
    next_addr += persisted_region_size;

    tracing::trace!(next_addr, "loading the kernel");

    // The aarch64 Linux kernel image is most commonly found as a flat binary with a
    // header rather than an ELF.
    // DeviceTree is generated dynamically by the boot shim.
    let initrd_address_type = InitrdAddressType::AfterKernel;
    let initrd_config = InitrdConfig {
        initrd_address: initrd_address_type,
        initrd: initrd.unwrap_or_default(),
    };
    let device_tree_blob = None;
    let crate::linux::LoadInfo {
        kernel:
            KernelInfo {
                gpa: kernel_base,
                size: kernel_size,
                entrypoint: kernel_entry_point,
            },
        initrd: initrd_info,
        dtb,
    } = load_kernel_and_initrd_arm64(
        importer,
        kernel_image,
        next_addr,
        Some(initrd_config),
        device_tree_blob,
    )
    .map_err(Error::Kernel)?;

    assert!(
        dtb.is_none(),
        "DeviceTree is generated dynamically by the boot shim."
    );

    tracing::trace!(kernel_base, "kernel loaded");

    let InitrdInfo {
        gpa: initrd_gpa,
        size: initrd_size,
    } = if let Some(initrd_info) = initrd_info {
        assert!(initrd_address_type == InitrdAddressType::AfterKernel);
        next_addr = initrd_info.gpa + initrd_info.size;
        initrd_info
    } else {
        next_addr = kernel_base + kernel_size;
        InitrdInfo { gpa: 0, size: 0 }
    };

    next_addr = align_up_to_page_size(next_addr);

    tracing::trace!(next_addr, "loading the boot shim");

    let crate::elf::LoadInfo {
        minimum_address_used: shim_base_addr,
        next_available_address: mut next_addr,
        entrypoint: shim_entry_point,
    } = crate::elf::load_static_elf(
        importer,
        shim,
        0,
        next_addr,
        false,
        BootPageAcceptance::Exclusive,
        "underhill-boot-shim",
    )
    .map_err(Error::Shim)?;

    tracing::trace!(shim_base_addr, "boot shim loaded");

    tracing::trace!(next_addr, "loading the command line");

    let cmdline_base = next_addr;
    let (cmdline, policy) = match command_line {
        CommandLineType::Static(val) => (val, CommandLinePolicy::STATIC),
        CommandLineType::HostAppendable(val) => (val, CommandLinePolicy::APPEND_CHOSEN),
    };

    if cmdline.len() > COMMAND_LINE_SIZE {
        return Err(Error::CommandLineSize(cmdline.len()));
    }

    let mut static_command_line = [0; COMMAND_LINE_SIZE];
    static_command_line[..cmdline.len()].copy_from_slice(cmdline.as_bytes());
    let paravisor_command_line = ParavisorCommandLine {
        policy,
        static_command_line_len: cmdline.len() as u16,
        static_command_line,
    };

    importer.import_pages(
        cmdline_base / HV_PAGE_SIZE,
        1,
        "underhill-command-line",
        BootPageAcceptance::Exclusive,
        paravisor_command_line.as_bytes(),
    )?;

    next_addr += HV_PAGE_SIZE;

    tracing::trace!(next_addr, "loading the boot shim parameters");

    let shim_params_base = next_addr;
    let shim_params_size = HV_PAGE_SIZE;

    next_addr += shim_params_size;

    let parameter_region_size = PARAVISOR_VTL2_CONFIG_REGION_PAGE_COUNT_MAX * HV_PAGE_SIZE;
    let parameter_region_start = next_addr;
    next_addr += parameter_region_size;

    tracing::debug!(parameter_region_start);

    // Reserve 8K for the bootshim log buffer.
    let bootshim_log_size = HV_PAGE_SIZE * 2;
    let bootshim_log_start = next_addr;
    next_addr += bootshim_log_size;

    importer.import_pages(
        bootshim_log_start / HV_PAGE_SIZE,
        bootshim_log_size / HV_PAGE_SIZE,
        "ohcl-boot-shim-log-buffer",
        BootPageAcceptance::Exclusive,
        &[],
    )?;

    // Reserve 16 pages for a bootshim heap. This is only used to parse the
    // protobuf payload from the previous instance in a servicing boot.
    //
    // Import these pages as it greatly simplifies the early startup code in the
    // bootshim for isolated guests. This allows the bootshim to use these pages
    // early on without extra acceptance calls.
    let heap_start = next_addr;
    let heap_size = 16 * HV_PAGE_SIZE;
    importer.import_pages(
        heap_start / HV_PAGE_SIZE,
        heap_size / HV_PAGE_SIZE,
        "ohcl-boot-shim-heap",
        BootPageAcceptance::Exclusive,
        &[],
    )?;
    next_addr += heap_size;

    // The end of memory used by the loader, excluding pagetables.
    let end_of_underhill_mem = next_addr;

    // Page tables live at the end of the VTL2 imported region, which allows it
    // to be relocated separately.
    let page_table_base_page_count = 5;
    let page_table_dynamic_page_count = 2 * page_table_base_page_count;
    let page_table_page_count = page_table_base_page_count + page_table_dynamic_page_count;
    let page_table_region_size = HV_PAGE_SIZE * page_table_page_count;
    let page_table_region_start = next_addr;
    next_addr += page_table_region_size;

    tracing::debug!(page_table_region_start, page_table_region_size);

    let next_addr = next_addr;

    // The memory used by the loader must be smaller than the memory available.
    if next_addr > memory_start_address + memory_size {
        return Err(Error::NotEnoughMemory(next_addr - memory_start_address));
    }

    // Shim parameters for locations are relative to the base of where the shim is loaded.
    let calculate_shim_offset = |addr: u64| -> i64 { addr.wrapping_sub(shim_base_addr) as i64 };
    let initrd_crc = crc32fast::hash(initrd.unwrap_or(&[]));
    let shim_params = ShimParamsRaw {
        kernel_entry_offset: calculate_shim_offset(kernel_entry_point),
        cmdline_offset: calculate_shim_offset(cmdline_base),
        initrd_offset: calculate_shim_offset(initrd_gpa),
        initrd_size,
        initrd_crc,
        supported_isolation_type: match importer.isolation_config().isolation_type {
            IsolationType::None | IsolationType::Vbs => {
                loader_defs::shim::SupportedIsolationType::VBS
            }
            _ => panic!("only None and VBS are supported for ARM64"),
        },
        memory_start_offset: calculate_shim_offset(memory_start_address),
        memory_size,
        parameter_region_offset: calculate_shim_offset(parameter_region_start),
        parameter_region_size,
        vtl2_reserved_region_offset: 0,
        vtl2_reserved_region_size: 0,
        sidecar_offset: 0,
        sidecar_size: 0,
        sidecar_entry_offset: 0,
        used_start: calculate_shim_offset(memory_start_address),
        used_end: calculate_shim_offset(next_addr),
        bounce_buffer_start: 0,
        bounce_buffer_size: 0,
        log_buffer_start: calculate_shim_offset(bootshim_log_start),
        log_buffer_size: bootshim_log_size,
        heap_start_offset: calculate_shim_offset(heap_start),
        heap_size,
        persisted_state_region_offset: calculate_shim_offset(persisted_region_base),
        persisted_state_region_size: persisted_region_size,
    };

    importer
        .import_pages(
            shim_params_base / HV_PAGE_SIZE,
            shim_params_size / HV_PAGE_SIZE,
            "underhill-shim-params",
            BootPageAcceptance::Exclusive,
            shim_params.as_bytes(),
        )
        .map_err(Error::Importer)?;

    let mut measured_config = ParavisorMeasuredVtl0Config {
        magic: ParavisorMeasuredVtl0Config::MAGIC,
        ..FromZeros::new_zeroed()
    };

    if let Some((uefi, vp_context)) = &supports_uefi {
        measured_config.supported_vtl0.set_uefi_supported(true);
        let vp_context_page = PARAVISOR_VTL0_MEASURED_CONFIG_BASE_PAGE_AARCH64 + 1;
        measured_config.uefi_info = UefiInfo {
            firmware: PageRegionDescriptor {
                base_page_number: uefi.firmware_base / HV_PAGE_SIZE,
                page_count: uefi.total_size / HV_PAGE_SIZE,
            },
            vtl0_vp_context: PageRegionDescriptor {
                base_page_number: vp_context_page,
                page_count: 1,
            },
        };

        // Deposit the UEFI vp context.
        importer.import_pages(
            vp_context_page,
            1,
            "openhcl-uefi-vp-context",
            BootPageAcceptance::Exclusive,
            vp_context,
        )?;
    }

    importer
        .import_pages(
            PARAVISOR_VTL0_MEASURED_CONFIG_BASE_PAGE_AARCH64,
            1,
            "underhill-measured-config",
            BootPageAcceptance::Exclusive,
            measured_config.as_bytes(),
        )
        .map_err(Error::Importer)?;

    tracing::trace!(page_table_region_start, "loading the page tables");

    let memory_attribute_indirection = MemoryAttributeIndirectionEl1([
        MemoryAttributeEl1::Device_nGnRnE,
        MemoryAttributeEl1::Normal_NonCacheable,
        MemoryAttributeEl1::Normal_WriteThrough,
        MemoryAttributeEl1::Normal_WriteBack,
        MemoryAttributeEl1::Device_nGnRnE,
        MemoryAttributeEl1::Device_nGnRnE,
        MemoryAttributeEl1::Device_nGnRnE,
        MemoryAttributeEl1::Device_nGnRnE,
    ]);
    let mut page_tables: Vec<u8> = vec![0; page_table_region_size as usize];
    let page_tables = page_table::aarch64::build_identity_page_tables_aarch64(
        page_table_region_start,
        memory_start_address,
        memory_size,
        memory_attribute_indirection,
        page_tables.as_mut_slice(),
    );
    assert!((page_tables.len() as u64).is_multiple_of(HV_PAGE_SIZE));
    let page_table_page_base = page_table_region_start / HV_PAGE_SIZE;
    assert!(page_tables.len() as u64 <= page_table_region_size);
    assert!(page_table_region_size as usize > page_tables.len());

    if with_relocation {
        // Indicate relocation information. Don't include page table region.
        importer.relocation_region(
            memory_start_address,
            end_of_underhill_mem - memory_start_address,
            Arm64PageSize::Large.into(),
            PARAVISOR_DEFAULT_MEMORY_BASE_ADDRESS,
            1 << 48,
            true,
            false,
            0, // BSP
        )?;

        // Tell the loader page table relocation information.
        importer.page_table_relocation(
            page_table_region_start,
            page_table_region_size / HV_PAGE_SIZE,
            page_tables.len() as u64 / HV_PAGE_SIZE,
            0,
        )?;
    }

    importer.import_pages(
        page_table_page_base,
        page_table_page_count,
        "underhill-page-tables",
        BootPageAcceptance::Exclusive,
        page_tables,
    )?;

    tracing::trace!("Importing register state");

    let mut import_reg = |register| {
        importer
            .import_vp_register(register)
            .map_err(Error::Importer)
    };

    // Set %X0 to relative location of boot_params_base
    let relative_boot_params_base = shim_params_base - shim_base_addr;
    import_reg(AArch64Register::X0(relative_boot_params_base).into())?;

    // Set %pc to the shim entry point.
    import_reg(AArch64Register::Pc(shim_entry_point).into())?;

    // System registers

    import_reg(AArch64Register::Cpsr(Cpsr64::new().with_sp(true).with_el(1).into()).into())?;

    // This is what Hyper-V uses. qemu/KVM, and qemu/max use slightly
    // different flags.
    // KVM sets these in addition to what the Hyper-V uses:
    //
    // .with_sa(true)
    // .with_itd(true)
    // .with_sed(true)
    //
    // Windows sets:
    //
    // .with_sa(true)
    // .with_sa0(true)
    // .with_n_aa(true)
    // .with_sed(true)
    // .with_dze(true)
    // .with_en_ib(true)
    // .with_dssbs(true)
    //
    // Maybe could enforce the `s`tack `a`lignment, here, too. Depends on
    // the compiler generating code aligned accesses for the stack.
    //
    // Hyper-V sets:
    import_reg(
        AArch64Register::SctlrEl1(
            SctlrEl1::new()
                // MMU enable for EL1&0 stage 1 address translation.
                // It can be turned off in VTL2 for debugging.
                // The family of the `at` instructions and the `PAR_EL1` register are
                // useful for debugging MMU issues.
                .with_m(true)
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
        )
        .into(),
    )?;

    // Hyper-V UEFI and qemu/KVM use the same value for TCR_EL1.
    // They set `t0sz` to `28` as they map memory pretty low.
    // In the paravisor case, need more flexibility.
    // For the details, refer to the "Learning the architecture" series
    // on the ARM website.
    import_reg(
        AArch64Register::TcrEl1(
            TranslationControlEl1::new()
                .with_t0sz(0x11)
                .with_irgn0(1)
                .with_orgn0(1)
                .with_sh0(3)
                .with_tg0(TranslationGranule0::TG_4KB)
                // Disable TTBR1_EL1 walks (i.e. the upper half).
                .with_epd1(1)
                // Due to erratum #822227, need to set a valid TG1 regardless of EPD1.
                .with_tg1(TranslationGranule1::TG_4KB)
                .with_ips(IntermPhysAddrSize::IPA_48_BITS_256_TB)
                .into(),
        )
        .into(),
    )?;

    // The Memory Attribute Indirection
    import_reg(AArch64Register::MairEl1(memory_attribute_indirection.into()).into())?;
    import_reg(
        AArch64Register::Ttbr0El1(
            TranslationBaseEl1::new()
                .with_baddr(page_table_region_start)
                .into(),
        )
        .into(),
    )?;

    // VBAR is in the undefined state, setting it to 0 albeit
    // without the vector exception table. The shim can configure that on its own
    // if need be.
    import_reg(AArch64Register::VbarEl1(0).into())?;

    // Load parameter regions.
    let config_region_page_base = parameter_region_start / HV_PAGE_SIZE;

    // Slit
    let slit_page_base = config_region_page_base + PARAVISOR_CONFIG_SLIT_PAGE_INDEX;
    let slit_parameter_area = importer.create_parameter_area(
        slit_page_base,
        PARAVISOR_CONFIG_SLIT_SIZE_PAGES as u32,
        "underhill-slit",
    )?;
    importer.import_parameter(slit_parameter_area, 0, IgvmParameterType::Slit)?;

    // Pptt
    let pptt_page_base = config_region_page_base + PARAVISOR_CONFIG_PPTT_PAGE_INDEX;
    let pptt_parameter_area = importer.create_parameter_area(
        pptt_page_base,
        PARAVISOR_CONFIG_PPTT_SIZE_PAGES as u32,
        "underhill-pptt",
    )?;
    importer.import_parameter(pptt_parameter_area, 0, IgvmParameterType::Pptt)?;

    // device tree
    let dt_page_base = config_region_page_base + PARAVISOR_CONFIG_DEVICE_TREE_PAGE_INDEX;
    let dt_parameter_area = importer.create_parameter_area(
        dt_page_base,
        PARAVISOR_CONFIG_DEVICE_TREE_SIZE_PAGES as u32,
        "underhill-device-tree",
    )?;
    importer.import_parameter(dt_parameter_area, 0, IgvmParameterType::DeviceTree)?;

    let vtl2_measured_config = ParavisorMeasuredVtl2Config {
        magic: ParavisorMeasuredVtl2Config::MAGIC,
        vtom_offset_bit: 0,
        padding: [0; 7],
    };

    importer
        .import_pages(
            config_region_page_base + PARAVISOR_MEASURED_VTL2_CONFIG_PAGE_INDEX,
            PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES,
            "underhill-vtl2-measured-config",
            BootPageAcceptance::Exclusive,
            vtl2_measured_config.as_bytes(),
        )
        .map_err(Error::Importer)?;

    let imported_region_base =
        config_region_page_base + PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_PAGE_INDEX;

    importer.set_imported_regions_config_page(imported_region_base);

    Ok(())
}
