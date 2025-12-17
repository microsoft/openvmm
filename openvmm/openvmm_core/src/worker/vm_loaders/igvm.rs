// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Loader implementation to load IGVM files.

use guestmem::GuestMemory;
use hvdef::HV_PAGE_SIZE;
use openvmm_defs::config::SerialInformation;
use openvmm_defs::config::Vtl2BaseAddressType;
use igvm::IgvmDirectiveHeader;
use igvm::IgvmFile;
use igvm::IgvmPlatformHeader;
use igvm::IgvmRelocatableRegion;
use igvm::page_table::CpuPagingState;
use igvm_defs::IGVM_VHS_MEMORY_MAP_ENTRY;
use igvm_defs::IGVM_VHS_MEMORY_RANGE;
use igvm_defs::IGVM_VHS_MMIO_RANGES;
use igvm_defs::IGVM_VHS_PARAMETER;
use igvm_defs::IGVM_VHS_PARAMETER_INSERT;
use igvm_defs::IgvmPageDataType;
use igvm_defs::IgvmPlatformType;
use loader::importer::Aarch64Register;
use loader::importer::BootPageAcceptance;
use loader::importer::GuestArch;
use loader::importer::ImageLoad;
use loader::importer::StartupMemoryType;
use loader::importer::TableRegister;
use loader::importer::X86Register;
use memory_range::MemoryRange;
use memory_range::subtract_ranges;
use range_map_vec::RangeMap;
use std::collections::HashMap;
use std::ffi::CString;
use std::io::Read;
use std::io::Seek;
use thiserror::Error;
use virt::PageVisibility;
use vm_loader::Loader;
use vm_topology::memory::MemoryLayout;
use vm_topology::memory::MemoryRangeWithNode;
use vm_topology::processor::ArchTopology;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::aarch64::Aarch64Topology;
use vm_topology::processor::x86::X86Topology;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub enum Error {
    #[error("command line is not a valid C string")]
    InvalidCommandLine(#[source] std::ffi::NulError),
    #[error("failed to read igvm file")]
    Igvm(#[source] std::io::Error),
    #[error("invalid igvm file")]
    InvalidIgvmFile(#[source] igvm::Error),
    #[error("loader error")]
    Loader(#[source] anyhow::Error),
    #[error("parameter too large for parameter area")]
    ParameterTooLarge,
    #[error("relocation not supported in igvm file")]
    RelocationNotSupported,
    #[error("multiple igvm relocation headers specified in the file")]
    MultipleIgvmRelocationHeaders,
    #[error("relocated base address is not supported by relocation header {file_relocation:?}")]
    RelocationBaseInvalid {
        file_relocation: IgvmRelocatableRegion,
    },
    #[error("page table relocation header not specified")]
    NoPageTableRelocationHeader,
    #[error("vp index does not describe the BSP in relocation headers")]
    RelocationVpIndex,
    #[error("vtl does not target vtl2 in relocation headers")]
    RelocationVtl,
    #[error("page table builder")]
    PageTableBuilder(#[source] igvm::page_table::Error),
    #[error("no vtl2 memory range in memory layout")]
    NoVtl2MemoryRange,
    #[error("no vtl2 memory source in igvm file")]
    Vtl2MemorySource,
    #[error("invalid memory config")]
    MemoryConfig(#[source] vm_topology::memory::Error),
    #[error("not enough physical address bits to allocate vtl2 range")]
    NotEnoughPhysicalAddressBits,
    #[error("building device tree for partition failed")]
    DeviceTree(fdt::builder::Error),
    #[error("supplied vtl2 memory {0} is not aligned to 2MB")]
    Vtl2MemoryAligned(u64),
    #[error("supplied vtl2 memory {0} is smaller than igvm file VTL2 range {1}")]
    Vtl2MemoryTooSmall(u64, u64),
    #[error("unsupported guest architecture")]
    UnsupportedGuestArch,
    #[error("igvm file does not support vbs")]
    NoVbsSupport,
    #[error("vp context for lower VTL not supported")]
    LowerVtlContext,
    #[error("missing required memory range {0}")]
    MissingRequiredMemory(MemoryRange),
    #[error("IGVM file requires at least two mmio ranges")]
    UnsupportedMmio,
}

fn from_memory_range(range: &MemoryRange) -> IGVM_VHS_MEMORY_RANGE {
    assert!(range.len().is_multiple_of(HV_PAGE_SIZE));
    IGVM_VHS_MEMORY_RANGE {
        starting_gpa_page_number: range.start() / HV_PAGE_SIZE,
        number_of_pages: range.len() / HV_PAGE_SIZE,
    }
}

fn memory_map_entry(range: &MemoryRange) -> IGVM_VHS_MEMORY_MAP_ENTRY {
    assert!(range.len().is_multiple_of(HV_PAGE_SIZE));
    IGVM_VHS_MEMORY_MAP_ENTRY {
        starting_gpa_page_number: range.start() / HV_PAGE_SIZE,
        number_of_pages: range.len() / HV_PAGE_SIZE,
        entry_type: igvm_defs::MemoryMapEntryType::MEMORY,
        flags: 0,
        reserved: 0,
    }
}

fn from_igvm_vtl(vtl: igvm::hv_defs::Vtl) -> hvdef::Vtl {
    match vtl {
        igvm::hv_defs::Vtl::Vtl0 => hvdef::Vtl::Vtl0,
        igvm::hv_defs::Vtl::Vtl1 => hvdef::Vtl::Vtl1,
        igvm::hv_defs::Vtl::Vtl2 => hvdef::Vtl::Vtl2,
    }
}

/// Read and parse an IgvmFile from a File. This assumes the file is a VBS IGVM
/// file.
pub fn read_igvm_file(mut file: &std::fs::File) -> Result<IgvmFile, Error> {
    let mut file_contents = Vec::new();
    file.rewind().map_err(Error::Igvm)?;
    file.read_to_end(&mut file_contents).map_err(Error::Igvm)?;

    let igvm_file = IgvmFile::new_from_binary(&file_contents, Some(igvm::IsolationType::Vbs))
        .map_err(Error::InvalidIgvmFile)?;

    Ok(igvm_file)
}

/// Extract the vbs supported platform header from an igvm file.
fn vbs_platform_header(igvm_file: &IgvmFile) -> Result<&IgvmPlatformHeader, Error> {
    igvm_file
        .platforms()
        .iter()
        .find(|header| {
            let IgvmPlatformHeader::SupportedPlatform(info) = header;
            info.platform_type == IgvmPlatformType::VSM_ISOLATION
        })
        .ok_or(Error::NoVbsSupport)
}

/// Determine if the given `igvm_file` supports relocations or not.
pub fn supports_relocations(igvm_file: &IgvmFile) -> bool {
    let (mask, _max_vtl) = match vbs_platform_header(igvm_file).unwrap() {
        IgvmPlatformHeader::SupportedPlatform(info) => {
            debug_assert_eq!(info.platform_type, IgvmPlatformType::VSM_ISOLATION);
            (info.compatibility_mask, info.highest_vtl)
        }
    };

    igvm_file.relocations(mask).0.is_some()
}

/// Determine the VTL2 memory size encoded in the file by looking for a
/// [`IgvmDirectiveHeader::RequiredMemory`] structure is looked for, with the
/// flag set for vtl2_protectable.
pub fn vtl2_memory_info(igvm_file: &IgvmFile) -> Result<MemoryRange, Error> {
    let (mask, _max_vtl) = match vbs_platform_header(igvm_file)? {
        IgvmPlatformHeader::SupportedPlatform(info) => {
            debug_assert_eq!(info.platform_type, IgvmPlatformType::VSM_ISOLATION);
            (info.compatibility_mask, info.highest_vtl)
        }
    };

    let mut required_memory = None;

    for header in igvm_file.directives().iter().filter(|header| {
        header
            .compatibility_mask()
            .map(|header_mask| header_mask & mask == mask)
            .unwrap_or(true)
    }) {
        if let IgvmDirectiveHeader::RequiredMemory {
            gpa,
            compatibility_mask: _,
            number_of_bytes,
            vtl2_protectable: true,
        } = *header
        {
            required_memory = Some(MemoryRange::new(gpa..gpa + number_of_bytes as u64));
            break;
        }
    }

    match required_memory {
        Some(range) => Ok(range),
        None => Err(Error::Vtl2MemorySource),
    }
}

/// Determine a location to allocate VTL2 memory, based on VM information and a
/// provided `igvm_file`.
pub fn vtl2_memory_range(
    physical_address_size: u8,
    mem_size: u64,
    mmio_gaps: &[MemoryRange],
    igvm_file: &IgvmFile,
    vtl2_size: Option<u64>,
) -> Result<MemoryRange, Error> {
    let (mask, _max_vtl) = match vbs_platform_header(igvm_file)? {
        IgvmPlatformHeader::SupportedPlatform(info) => {
            debug_assert_eq!(info.platform_type, IgvmPlatformType::VSM_ISOLATION);
            (info.compatibility_mask, info.highest_vtl)
        }
    };

    let relocs = igvm_file.relocations(mask);

    // Use the required memory struct as the hint for how large the file needs
    // for vtl2 mem.
    let igvm_size = vtl2_memory_info(igvm_file)?.len();

    // TODO: only supports single relocation region, since that's what Underhill
    //       does
    let reloc_region = relocs.0.ok_or(Error::RelocationNotSupported)?[0].clone();

    let alignment = reloc_region.relocation_alignment;

    let size = match vtl2_size {
        Some(vtl2_size) => {
            const TWO_MB: u64 = 2 * 1024 * 1024;
            if vtl2_size % TWO_MB != 0 {
                return Err(Error::Vtl2MemoryAligned(vtl2_size));
            }

            if vtl2_size < igvm_size {
                return Err(Error::Vtl2MemoryTooSmall(vtl2_size, igvm_size));
            }

            vtl2_size
        }
        None => {
            // Use IGVM provided size
            igvm_size
        }
    };

    let align_base = |base| -> u64 { (base + alignment - 1) & !(alignment - 1) };

    // Use one bit below the maximum possible address, as the VTL0 alias map
    // will use the highest available bit of the physical address space.
    let physical_address_size = physical_address_size - 1;

    // Create an initial memory layout to determine the highest used address.
    let dummy_layout = MemoryLayout::new(mem_size, mmio_gaps, None).map_err(Error::MemoryConfig)?;

    // TODO: Underhill kernel panics if loaded at 32TB or higher. Restrict the
    // max address to 32TB until this is fixed.
    const MAX_ADDR_32TB: u64 = 32u64 << 40; // 0x2000_0000_0000 bytes
    let max_physical_address = 1 << physical_address_size;
    let max_physical_address = max_physical_address.min(MAX_ADDR_32TB);

    // With more than two mmio gaps, it's harder to reason about which space is
    // free or not in the address space to allocate a VTL2 range. Take a
    // shortcut and place VTL2 above the end of ram or mmio.
    let (min_addr, max_addr) = (dummy_layout.end_of_ram_or_mmio(), max_physical_address);

    let aligned_min_addr = align_base(min_addr);
    let aligned_max_addr = (max_addr / alignment) * alignment;

    assert!(aligned_min_addr >= reloc_region.minimum_relocation_gpa);
    assert!(aligned_max_addr <= reloc_region.maximum_relocation_gpa);

    // It's possible that the min_addr is above the physical address size of the
    // system. Fail now as mapping ram would fail later.
    if aligned_min_addr >= aligned_max_addr {
        return Err(Error::NotEnoughPhysicalAddressBits);
    }

    tracing::trace!(min_addr, aligned_min_addr, max_addr, aligned_max_addr);

    // Select a random base within the alignment
    let possible_bases = (aligned_max_addr - aligned_min_addr) / alignment;
    let mut num: u64 = 0;
    getrandom::fill(num.as_mut_bytes()).expect("crng failure");
    let selected_base = num % (possible_bases - 1);
    let selected_addr = aligned_min_addr + (selected_base * alignment);
    tracing::trace!(possible_bases, selected_base, selected_addr);

    Ok(MemoryRange::new(selected_addr..(selected_addr + size)))
}

/// Build a device tree representing the whole guest partition.
fn build_device_tree(
    processor_topology: &ProcessorTopology<X86Topology>,
    mem_layout: &MemoryLayout,
    all_ram: &[MemoryRangeWithNode],
    vtl2_protectable_ram: &[MemoryRange],
    vtl2_base_address: Vtl2BaseAddressType,
    command_line: &str,
    with_vmbus_redirect: bool,
    com_serial: Option<SerialInformation>,
    entropy: Option<&[u8]>,
) -> Result<Vec<u8>, fdt::builder::Error> {
    let mut buf = vec![0; HV_PAGE_SIZE as usize * 256];

    let mut builder = fdt::builder::Builder::new(fdt::builder::BuilderConfig {
        blob_buffer: buf.as_mut_slice(),
        string_table_cap: 1024,
        memory_reservations: &[],
    })?;
    let p_address_cells = builder.add_string("#address-cells")?;
    let p_size_cells = builder.add_string("#size-cells")?;
    let p_model = builder.add_string("model")?;
    let p_reg = builder.add_string("reg")?;
    let p_ranges = builder.add_string("ranges")?;
    let p_device_type = builder.add_string("device_type")?;
    let p_status = builder.add_string("status")?;
    let p_igvm_type = builder.add_string(igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY)?;
    let p_compatible = builder.add_string("compatible")?;
    let p_numa_node_id = builder.add_string("numa-node-id")?;
    let p_vmbus_connection_id = builder.add_string("microsoft,message-connection-id")?;
    let p_vtl = builder.add_string(igvm_defs::dt::IGVM_DT_VTL_PROPERTY)?;
    let p_bootargs = builder.add_string("bootargs")?;
    let p_clock_frequency = builder.add_string("clock-frequency")?;
    let p_current_speed = builder.add_string("current-speed")?;
    let p_interrupts = builder.add_string("interrupts")?;

    let mut cpus = builder
        .start_node("")?
        .add_u32(p_address_cells, 2)? // 64bit
        .add_u32(p_size_cells, 2)? // 64bit
        .add_str(p_model, "microsoft,hyperv")?
        .start_node("cpus")?
        .add_u32(p_address_cells, 1)?
        .add_u32(p_size_cells, 0)?;

    // Add a CPU node for each VP.
    for proc in processor_topology.vps_arch() {
        let name = format!("cpu@{:x}", proc.base.vp_index.index() + 1);
        cpus = cpus
            .start_node(name.as_ref())?
            .add_str(p_device_type, "cpu")?
            .add_u32(p_reg, proc.apic_id)?
            .add_u32(p_numa_node_id, proc.base.vnode)?
            .add_str(p_status, "okay")?
            .end_node()?;
    }

    let mut root = cpus.end_node()?;

    let (memory_map, vnodes) = build_memory_map(all_ram, vtl2_protectable_ram);

    // Build the memory entries in reverse order to require the underhill fdt
    // parser to sort them correctly.
    for (entry, vnode) in memory_map.iter().zip(vnodes.iter()).rev() {
        let start_address = entry.starting_gpa_page_number * HV_PAGE_SIZE;
        let size = entry.number_of_pages * HV_PAGE_SIZE;
        let name = format!("memory@{:x}", start_address);
        let mut mem = root.start_node(&name)?;
        mem = mem.add_str(p_device_type, "memory")?;
        mem = mem.add_u64_array(p_reg, &[start_address, size])?;
        mem = mem.add_u32(p_igvm_type, entry.entry_type.0 as u32)?;
        mem = mem.add_u32(p_numa_node_id, *vnode)?;
        root = mem.end_node()?;
    }

    // Linux requires vmbus to be under a simple-bus node.
    let mut simple_bus = root
        .start_node("bus")?
        .add_str(p_compatible, "simple-bus")?
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?
        .add_prop_array(p_ranges, &[])?;

    // Determine how much mmio this system has. 2 or less gaps are reported to
    // VTL0. The 3rd and/or 4th gap will be reported to VTL2. Any more are
    // ignored.
    let mut mmio_chunks = mem_layout.mmio().chunks(2);

    let extract_ranges = |mmio: Option<&[MemoryRange]>| -> Vec<u64> {
        let mut ranges = Vec::new();

        if let Some(mmio) = mmio {
            for entry in mmio {
                ranges.push(entry.start());
                ranges.push(entry.start());
                ranges.push(entry.len());
            }
        }
        ranges
    };

    let ranges_vtl0 = extract_ranges(mmio_chunks.next());
    let ranges_vtl2 = extract_ranges(mmio_chunks.next());

    // VTL0 vmbus root device
    let vmbus_vtl0_name = if ranges_vtl0.is_empty() {
        "vmbus-vtl0".into()
    } else {
        format!("vmbus-vtl0@{:x}", ranges_vtl0[0])
    };
    let vmbus_vtl0 = simple_bus.start_node(&vmbus_vtl0_name)?;
    simple_bus = vmbus_vtl0
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?
        .add_str(p_compatible, "microsoft,vmbus")?
        .add_u64_array(p_ranges, &ranges_vtl0)?
        .add_u32(p_vtl, 0)?
        .add_u32(p_vmbus_connection_id, 1)?
        .end_node()?;

    // VTL2 vmbus root device
    let vmbus_vtl2_name = if ranges_vtl2.is_empty() {
        "vmbus-vtl2".into()
    } else {
        format!("vmbus-vtl2@{:x}", ranges_vtl2[0])
    };
    let vmbus_vtl2 = simple_bus.start_node(&vmbus_vtl2_name)?;
    simple_bus = vmbus_vtl2
        .add_u32(p_address_cells, 2)?
        .add_u32(p_size_cells, 2)?
        .add_str(p_compatible, "microsoft,vmbus")?
        .add_u64_array(p_ranges, &ranges_vtl2)?
        .add_u32(p_vtl, 2)?
        .add_u32(
            p_vmbus_connection_id,
            if with_vmbus_redirect {
                // TODO: is this value defined anywhere? can we pass it in instead?
                0x800074
            } else {
                4
            },
        )?
        .end_node()?;

    root = simple_bus.end_node()?;

    if let Some(serial_cfg) = com_serial {
        let mut io_port_bus = root
            .start_node("pio-bus")?
            .add_str(p_compatible, "x86-pio-bus")?
            .add_u32(p_address_cells, 1)?
            .add_u32(p_size_cells, 1)?
            .add_prop_array(p_ranges, &[])?;

        let serial_name = format!("serial@{:x}", serial_cfg.io_port);
        io_port_bus = io_port_bus
            .start_node(&serial_name)?
            .add_str(p_compatible, "ns16550")?
            .add_u32(p_clock_frequency, 0)?
            .add_u32(p_current_speed, 115200)?
            .add_u64_array(p_reg, &[serial_cfg.io_port.into(), 0x8])?
            .add_u64_array(p_interrupts, &[serial_cfg.irq.into()])?
            .end_node()?;

        root = io_port_bus.end_node()?;
    }

    // Chosen node - contains cmdline.
    root = root
        .start_node("chosen")?
        .add_str(p_bootargs, command_line)?
        .end_node()?;

    // openhcl node - contains memory allocation mode.
    let p_memory_allocation_mode = root.add_string("memory-allocation-mode")?;
    let p_memory_size = root.add_string("memory-size")?;
    let p_mmio_size = root.add_string("mmio-size")?;
    let p_vf_keep_alive_devs = root.add_string("device-types")?;
    let mut openhcl = root.start_node("openhcl")?;

    let memory_allocation_mode = match vtl2_base_address {
        Vtl2BaseAddressType::Vtl2Allocate { size } => {
            if let Some(size) = size {
                // Encode the size at the expected property.
                openhcl = openhcl.add_u64(p_memory_size, size)?;
            }

            // TODO: allow configuring more mmio size, but report 128 MB for
            // now.
            openhcl = openhcl.add_u64(p_mmio_size, 128 * 1024 * 1024)?;

            "vtl2"
        }
        _ => "host",
    };

    openhcl = openhcl.add_str(p_memory_allocation_mode, memory_allocation_mode)?;

    if let Some(entropy) = entropy {
        openhcl = openhcl
            .start_node("entropy")?
            .add_prop_array(p_reg, &[entropy])?
            .end_node()?;
    }

    // Indicate that NVMe keep-alive feature is supported by this VMM.
    openhcl = openhcl
        .start_node("keep-alive")?
        .add_str(p_vf_keep_alive_devs, "nvme")?
        .end_node()?;

    root = openhcl.end_node()?;

    let bytes_used = root
        .end_node()?
        .build(processor_topology.vp_arch(virt::VpIndex::BSP).apic_id)?;
    buf.truncate(bytes_used);

    Ok(buf)
}

#[derive(Clone, Copy)]
pub struct AcpiTables<'a> {
    pub madt: &'a [u8],
    pub srat: &'a [u8],
    pub slit: Option<&'a [u8]>,
    pub pptt: Option<&'a [u8]>,
}

/// The parameters to the [`load_igvm`] function.
pub struct LoadIgvmParams<'a, T: ArchTopology> {
    /// The IGVM file to load.
    pub igvm_file: &'a IgvmFile,
    /// The guest memory instance to access guest memory with.
    pub gm: &'a GuestMemory,
    /// The processor topology of the guest.
    pub processor_topology: &'a ProcessorTopology<T>,
    /// The memory layout of the guest.
    pub mem_layout: &'a MemoryLayout,
    /// The command line used to build the IGVM command line.
    pub cmdline: &'a str,
    /// The ACPI tables to report to the guest.
    pub acpi_tables: AcpiTables<'a>,
    /// The base address to load VTL2 at.
    pub vtl2_base_address: Vtl2BaseAddressType,
    /// The framebuffer base address, if set.
    pub vtl2_framebuffer_gpa_base: Option<u64>,
    /// Only load VTL2, do not load VTL0.
    pub vtl2_only: bool,
    /// Is vmbus redirection to VTL2 enabled for this guest.
    pub with_vmbus_redirect: bool,
    /// Should a com device be configured.
    pub com_serial: Option<SerialInformation>,
    /// Entropy
    pub entropy: Option<&'a [u8]>,
}

pub fn load_igvm(
    params: LoadIgvmParams<'_, vm_topology::processor::TargetTopology>,
) -> Result<
    (
        Vec<loader::importer::Register>,
        Vec<(MemoryRange, PageVisibility)>,
    ),
    Error,
> {
    #[cfg(guest_arch = "x86_64")]
    {
        load_igvm_x86(params)
    }
    #[cfg(guest_arch = "aarch64")]
    {
        load_igvm_aarch64(params)
    }
}

/// Load the given IGVM file.
///
/// TODO: only supports underhill for now, with assumptions that the file always
/// has VTL2 enabled.
#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
fn load_igvm_x86(
    params: LoadIgvmParams<'_, X86Topology>,
) -> Result<(Vec<X86Register>, Vec<(MemoryRange, PageVisibility)>), Error> {
    let LoadIgvmParams {
        igvm_file,
        gm,
        processor_topology,
        mem_layout,
        cmdline,
        acpi_tables,
        vtl2_base_address,
        vtl2_framebuffer_gpa_base,
        vtl2_only,
        with_vmbus_redirect,
        com_serial,
        entropy,
    } = params;

    let relocations_enabled = match vtl2_base_address {
        Vtl2BaseAddressType::File | Vtl2BaseAddressType::Vtl2Allocate { .. } => false,
        Vtl2BaseAddressType::Absolute(_) | Vtl2BaseAddressType::MemoryLayout { .. } => true,
    };

    // TODO: pass this through an IGVM parameter
    let cmdline = if let Some(vtl2_framebuffer_gpa_base) = vtl2_framebuffer_gpa_base {
        format!(
            "OPENHCL_FRAMEBUFFER_GPA_BASE={} {}",
            vtl2_framebuffer_gpa_base, cmdline
        )
    } else {
        cmdline.to_string()
    };

    let command_line = CString::new(cmdline).map_err(Error::InvalidCommandLine)?;

    let (mask, max_vtl) = match vbs_platform_header(igvm_file)? {
        IgvmPlatformHeader::SupportedPlatform(info) => {
            debug_assert_eq!(info.platform_type, IgvmPlatformType::VSM_ISOLATION);
            (info.compatibility_mask, info.highest_vtl)
        }
    };

    let (relocation_regions, mut page_table_fixup) = igvm_file.relocations(mask);

    // If relocations are being requested, the image must support it and it must
    // meet the image restrictions.
    let (relocation_region, relocation_offset) = if relocations_enabled {
        // Relocation support must exist in the file.
        match relocation_regions {
            Some(regions) => {
                // We expect a single relocation header that describes VTL2, and
                // a page table relocation region. The vp_index and vtl targeted
                // by these headers must both be the BSP and VTL2.

                if regions.len() != 1 {
                    // Only one relocation region is supported in the loader for
                    // now.
                    return Err(Error::MultipleIgvmRelocationHeaders);
                }

                let region = regions[0].clone();

                if !region.is_vtl2 {
                    return Err(Error::RelocationVtl);
                }

                // There must be a page table fixup region, as we expect both.
                if page_table_fixup.is_none() {
                    return Err(Error::NoPageTableRelocationHeader);
                }

                let page_table_fixup = page_table_fixup.as_ref().expect("is set");

                // Calculate the vtl2_base_address, based on the requested
                // address type.
                let vtl2_base_address = match vtl2_base_address {
                    Vtl2BaseAddressType::Absolute(addr) => addr,
                    Vtl2BaseAddressType::MemoryLayout { .. } => {
                        let vtl2_range = mem_layout.vtl2_range().ok_or(Error::NoVtl2MemoryRange)?;
                        vtl2_range.start()
                    }
                    Vtl2BaseAddressType::File | Vtl2BaseAddressType::Vtl2Allocate { .. } => {
                        unreachable!()
                    }
                };

                // Check that the supplied vtl2 base address is supported by the
                // file
                if !region.relocation_base_valid(vtl2_base_address) {
                    return Err(Error::RelocationBaseInvalid {
                        file_relocation: region,
                    });
                }

                tracing::trace!(vtl2_base_address);

                // Calculate the relocation offset. Only positive offsets are
                // currently supported, which underhill should already
                // constrain.
                assert!(vtl2_base_address >= region.base_gpa);
                let relocation_offset = Some(vtl2_base_address - region.base_gpa);

                if region.vp_index != 0 || page_table_fixup.vp_index != 0 {
                    return Err(Error::RelocationVpIndex);
                }

                if region.vtl != igvm::hv_defs::Vtl::Vtl2
                    || page_table_fixup.vtl != igvm::hv_defs::Vtl::Vtl2
                {
                    return Err(Error::RelocationVtl);
                }

                tracing::trace!(relocation_offset);

                (Some(region), relocation_offset)
            }
            None => {
                return Err(Error::RelocationNotSupported);
            }
        }
    } else {
        // No relocation requested, just use the PAGE_DATAs specified in the
        // file as-is.
        (None, None)
    };

    let max_vtl = max_vtl
        .try_into()
        .expect("igvm file should be valid after new_from_binary");

    let mut loader = Loader::new(gm.clone(), mem_layout, max_vtl);

    #[derive(Debug)]
    enum ParameterAreaState {
        /// Parameter area has been declared via a ParameterArea header.
        Allocated { data: Vec<u8>, max_size: u64 },
        /// Parameter area inserted and invalid to use.
        Inserted,
    }
    let mut parameter_areas: HashMap<u32, ParameterAreaState> = HashMap::new();

    // Import a parameter to the given parameter area.
    let import_parameter = |parameter_areas: &mut HashMap<u32, ParameterAreaState>,
                            info: &IGVM_VHS_PARAMETER,
                            parameter: &[u8]|
     -> Result<(), Error> {
        let (parameter_area, max_size) = match *parameter_areas
            .get_mut(&info.parameter_area_index)
            .expect("parameter area should be present")
        {
            ParameterAreaState::Allocated {
                ref mut data,
                max_size,
            } => (data, max_size),
            ParameterAreaState::Inserted => panic!("igvmfile is not valid"),
        };
        let offset = info.byte_offset as usize;
        let end_of_parameter = offset + parameter.len();

        if end_of_parameter > max_size as usize {
            // TODO: tracing for which parameter was too big?
            return Err(Error::ParameterTooLarge);
        }

        if parameter_area.len() < end_of_parameter {
            parameter_area.resize(end_of_parameter, 0);
        }

        parameter_area[offset..end_of_parameter].copy_from_slice(parameter);
        Ok(())
    };

    // Relocate a given gpa if relocations are enabled, and it falls within the VTL2 relocation region.
    let relocate_gpa = |gpa: u64| -> u64 {
        match (&relocation_offset, &relocation_region) {
            (Some(offset), Some(region)) if region.contains(gpa) => gpa + offset,
            _ => gpa,
        }
    };

    // Ensure required memory is present.
    let required_ram = igvm_file.directives().iter().filter_map(|header| {
        if let IgvmDirectiveHeader::RequiredMemory {
            gpa,
            compatibility_mask: _,
            number_of_bytes,
            vtl2_protectable: _,
        } = *header
        {
            let base = relocate_gpa(gpa);
            Some(MemoryRange::new(base..base + number_of_bytes as u64))
        } else {
            None
        }
    });

    let mut all_ram = mem_layout
        .ram()
        .iter()
        .cloned()
        .chain(
            mem_layout
                .vtl2_range()
                .map(|r| MemoryRangeWithNode { range: r, vnode: 0 }),
        )
        .collect::<Vec<_>>();

    all_ram.sort_by_key(|r| r.range.start());

    if let Some(range) = subtract_ranges(required_ram, all_ram.iter().map(|r| r.range)).next() {
        return Err(Error::MissingRequiredMemory(range));
    }

    // Anything requested is VTL2 protectable.
    let mut vtl2_protectable_ram = match vtl2_base_address {
        Vtl2BaseAddressType::File
        | Vtl2BaseAddressType::Absolute(_)
        | Vtl2BaseAddressType::MemoryLayout { .. } => igvm_file
            .directives()
            .iter()
            .filter_map(|header| {
                if let IgvmDirectiveHeader::RequiredMemory {
                    gpa,
                    compatibility_mask: _,
                    number_of_bytes,
                    vtl2_protectable: true,
                } = *header
                {
                    let base = relocate_gpa(gpa);
                    Some(MemoryRange::new(base..base + number_of_bytes as u64))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>(),
        Vtl2BaseAddressType::Vtl2Allocate { .. } => Vec::new(),
    };

    // If an extra VTL2 range is provided, add it to the protectable list.
    if let Some(range) = mem_layout.vtl2_range() {
        vtl2_protectable_ram.push(range);
    }

    vtl2_protectable_ram.sort_by_key(|r| r.start());

    let mut page_table_cpu_state: Option<CpuPagingState> = None;

    // If requested, filter to VTL2-related directives only.
    let pt_range = page_table_fixup.as_ref().map_or(MemoryRange::EMPTY, |x| {
        MemoryRange::new(x.gpa..x.gpa + x.size)
    });
    let directives = igvm_file.directives().iter().filter(|&header| {
        if !vtl2_only {
            true
        } else if let Some(reloc_region) = &relocation_region {
            // Remove directives for pages outside relocation regions, and for
            // registers for lower VTLs.
            match *header {
                IgvmDirectiveHeader::PageData { gpa, .. } => {
                    reloc_region.contains(gpa) || pt_range.contains_addr(gpa)
                }
                IgvmDirectiveHeader::X64VbsVpContext { vtl, .. } => vtl == igvm::hv_defs::Vtl::Vtl2,
                IgvmDirectiveHeader::AArch64VbsVpContext { vtl, .. } => {
                    vtl == igvm::hv_defs::Vtl::Vtl2
                }
                IgvmDirectiveHeader::ParameterInsert(IGVM_VHS_PARAMETER_INSERT {
                    gpa,
                    compatibility_mask: _,
                    parameter_area_index: _,
                }) => reloc_region.contains(gpa),
                IgvmDirectiveHeader::ParameterArea { .. }
                | IgvmDirectiveHeader::VpCount { .. }
                | IgvmDirectiveHeader::Srat { .. }
                | IgvmDirectiveHeader::Madt { .. }
                | IgvmDirectiveHeader::Slit { .. }
                | IgvmDirectiveHeader::Pptt { .. }
                | IgvmDirectiveHeader::MmioRanges { .. }
                | IgvmDirectiveHeader::MemoryMap { .. }
                | IgvmDirectiveHeader::CommandLine { .. }
                | IgvmDirectiveHeader::RequiredMemory { .. }
                | IgvmDirectiveHeader::SnpVpContext { .. }
                | IgvmDirectiveHeader::ErrorRange { .. }
                | IgvmDirectiveHeader::SnpIdBlock { .. }
                | IgvmDirectiveHeader::VbsMeasurement { .. }
                | IgvmDirectiveHeader::DeviceTree { .. }
                | IgvmDirectiveHeader::EnvironmentInfo { .. } => true,
                IgvmDirectiveHeader::X64NativeVpContext { .. } => {
                    todo!("native igvm type not supported yet")
                }
            }
        } else {
            panic!("no relocation region, cannot filter to VTL2");
        }
    });

    let mut page_data = PageDataBuffer::new();
    for header in directives {
        debug_assert!(header.compatibility_mask().unwrap_or(mask) & mask == mask);

        match *header {
            IgvmDirectiveHeader::PageData {
                gpa,
                compatibility_mask: _,
                flags,
                data_type,
                ref data,
            } => {
                debug_assert!((data.len() as u64).is_multiple_of(HV_PAGE_SIZE));

                // TODO: only 4k or empty page data supported right now
                assert!(data.len() as u64 == HV_PAGE_SIZE || data.is_empty());

                // If this is page table memory and relocations are being performed, then do not import it.
                // Keep the page data to be fixed up later after all headers have been imported.
                if relocations_enabled && page_table_fixup.as_ref().expect("is some").contains(gpa)
                {
                    page_table_fixup
                        .as_mut()
                        .expect("must have page table reloc")
                        .set_page_data(gpa, data)
                        .expect("gpa and len should be valid");
                    continue;
                }

                let acceptance = match data_type {
                    IgvmPageDataType::NORMAL => {
                        if flags.unmeasured() {
                            BootPageAcceptance::ExclusiveUnmeasured
                        } else if flags.shared() {
                            BootPageAcceptance::Shared
                        } else {
                            BootPageAcceptance::Exclusive
                        }
                    }
                    // TODO: other data types SNP / TDX only, unsupported
                    _ => todo!("unsupported IgvmPageDataType"),
                };

                if data.is_empty() {
                    page_data.zero(&mut loader, relocate_gpa(gpa), acceptance, HV_PAGE_SIZE)?;
                } else {
                    page_data.append(&mut loader, relocate_gpa(gpa), acceptance, data)?;
                }
            }
            IgvmDirectiveHeader::ParameterArea {
                number_of_bytes,
                parameter_area_index,
                ref initial_data,
            } => {
                debug_assert!(number_of_bytes % HV_PAGE_SIZE == 0);
                debug_assert!(
                    initial_data.is_empty() || initial_data.len() as u64 == number_of_bytes
                );

                // Allocate a new parameter area. It must not be already used.
                if parameter_areas
                    .insert(
                        parameter_area_index,
                        ParameterAreaState::Allocated {
                            data: initial_data.clone(),
                            max_size: number_of_bytes,
                        },
                    )
                    .is_some()
                {
                    panic!("IgvmFile is not valid, invalid invariant");
                }
            }
            IgvmDirectiveHeader::VpCount(ref info) => {
                let proc_count: u32 = processor_topology.vp_count();
                import_parameter(&mut parameter_areas, info, proc_count.as_bytes())?;
            }
            IgvmDirectiveHeader::Srat(ref info) => {
                import_parameter(&mut parameter_areas, info, acpi_tables.srat)?;
            }
            IgvmDirectiveHeader::Madt(ref info) => {
                import_parameter(&mut parameter_areas, info, acpi_tables.madt)?;
            }
            IgvmDirectiveHeader::Slit(ref info) => {
                if let Some(slit) = acpi_tables.slit {
                    import_parameter(&mut parameter_areas, info, slit)?;
                } else {
                    tracing::warn!("igvm file requested a SLIT, but no SLIT was provided")
                }
            }
            IgvmDirectiveHeader::Pptt(ref info) => {
                if let Some(pptt) = acpi_tables.pptt {
                    import_parameter(&mut parameter_areas, info, pptt)?;
                } else {
                    tracing::warn!("igvm file requested a PPTT, but no PPTT was provided")
                }
            }
            IgvmDirectiveHeader::MmioRanges(ref info) => {
                // Convert the OpenVMM format to the IGVM format
                // Any gaps above 2 are ignored.
                let mmio = mem_layout.mmio();
                if mmio.len() < 2 {
                    return Err(Error::UnsupportedMmio);
                }
                let mmio_ranges = IGVM_VHS_MMIO_RANGES {
                    mmio_ranges: [from_memory_range(&mmio[0]), from_memory_range(&mmio[1])],
                };
                import_parameter(&mut parameter_areas, info, mmio_ranges.as_bytes())?;
            }
            IgvmDirectiveHeader::MemoryMap(ref info) => {
                let (memory_map, _) = build_memory_map(&all_ram, &vtl2_protectable_ram);
                import_parameter(&mut parameter_areas, info, memory_map.as_bytes())?;
            }
            IgvmDirectiveHeader::CommandLine(ref info) => {
                import_parameter(&mut parameter_areas, info, command_line.as_bytes_with_nul())?;
            }
            IgvmDirectiveHeader::DeviceTree(ref info) => {
                let dt = build_device_tree(
                    processor_topology,
                    mem_layout,
                    &all_ram,
                    &vtl2_protectable_ram,
                    vtl2_base_address,
                    &String::from_utf8_lossy(command_line.as_bytes()),
                    with_vmbus_redirect,
                    com_serial,
                    entropy,
                )
                .map_err(Error::DeviceTree)?;
                import_parameter(&mut parameter_areas, info, &dt)?;
            }
            IgvmDirectiveHeader::RequiredMemory {
                gpa,
                compatibility_mask: _,
                number_of_bytes,
                vtl2_protectable,
            } => {
                let memory_type = if vtl2_protectable {
                    StartupMemoryType::Vtl2ProtectableRam
                } else {
                    StartupMemoryType::Ram
                };

                let gpa = relocate_gpa(gpa);

                loader
                    .verify_startup_memory_available(
                        gpa / HV_PAGE_SIZE,
                        number_of_bytes as u64 / HV_PAGE_SIZE,
                        memory_type,
                    )
                    .map_err(Error::Loader)?;
            }
            IgvmDirectiveHeader::EnvironmentInfo(ref info) => {
                let environment_info =
                    igvm_defs::IgvmEnvironmentInfo::new().with_memory_is_shared(false);
                import_parameter(&mut parameter_areas, info, environment_info.as_bytes())?;
            }
            IgvmDirectiveHeader::SnpVpContext { .. } => todo!("snp not supported"),
            IgvmDirectiveHeader::SnpIdBlock { .. } => todo!("snp not supported"),
            IgvmDirectiveHeader::VbsMeasurement { .. } => todo!("vbs not supported"),
            IgvmDirectiveHeader::X64VbsVpContext {
                vtl,
                ref registers,
                compatibility_mask: _,
            } => {
                if from_igvm_vtl(vtl) != max_vtl {
                    return Err(Error::LowerVtlContext);
                }

                let mut cr3: Option<u64> = None;
                let mut cr4: Option<u64> = None;

                for reg in registers.iter().map(|igvm_reg| {
                    let reg: X86Register = (*igvm_reg).into();
                    reg
                }) {
                    // Some registers may need to be relocated, depending on
                    // what is set in the IGVM header.

                    let reloc_reg = match reg {
                        X86Register::Gdtr(value) => match relocation_region {
                            Some(ref region) if region.apply_gdtr_offset => {
                                X86Register::Gdtr(TableRegister {
                                    base: relocate_gpa(value.base),
                                    ..value
                                })
                            }
                            _ => reg,
                        },
                        X86Register::Tr(_reg) => {
                            // NOTE: Skip TR as the loader doesn't actually load
                            //       it. The only usage is to set to the
                            //       architectural default anyways.
                            tracing::warn!("TR register load being skipped");
                            continue;
                        }
                        X86Register::Cr3(reg) => {
                            if let Some(offset) = relocation_offset {
                                // Save the original cr3 value to be used to fix
                                // up the page table later, and relocate cr3.
                                cr3 = Some(reg);

                                let page_table_fixup =
                                    page_table_fixup.as_ref().expect("should be some");

                                // should be verified by igvm file, but confirm.
                                assert!(page_table_fixup.contains(reg));

                                let reloc_cr3 = reg + offset;

                                X86Register::Cr3(reloc_cr3)
                            } else {
                                X86Register::Cr3(reg)
                            }
                        }
                        X86Register::Cr4(val) => {
                            if relocations_enabled {
                                // Save the value of Cr4 if relocations are
                                // being performed.
                                cr4 = Some(val);
                            }

                            reg
                        }

                        X86Register::Rip(rip) => match relocation_region {
                            Some(ref region) if region.apply_rip_offset => {
                                X86Register::Rip(relocate_gpa(rip))
                            }
                            _ => reg,
                        },

                        X86Register::Ds(_)
                        | X86Register::Es(_)
                        | X86Register::Fs(_)
                        | X86Register::Gs(_)
                        | X86Register::Ss(_)
                        | X86Register::Cs(_)
                        | X86Register::Cr0(_)
                        | X86Register::Efer(_)
                        | X86Register::Pat(_)
                        | X86Register::Rbp(_)
                        | X86Register::Rsi(_)
                        | X86Register::Rsp(_)
                        | X86Register::R8(_)
                        | X86Register::R9(_)
                        | X86Register::R10(_)
                        | X86Register::R11(_)
                        | X86Register::R12(_)
                        | X86Register::Rflags(_)
                        | X86Register::Idtr(_)
                        | X86Register::MtrrDefType(_)
                        | X86Register::MtrrFix64k00000(_)
                        | X86Register::MtrrFix16k80000(_)
                        | X86Register::MtrrPhysBase0(_)
                        | X86Register::MtrrPhysMask0(_)
                        | X86Register::MtrrPhysBase1(_)
                        | X86Register::MtrrPhysMask1(_)
                        | X86Register::MtrrPhysBase2(_)
                        | X86Register::MtrrPhysMask2(_)
                        | X86Register::MtrrPhysBase3(_)
                        | X86Register::MtrrPhysMask3(_)
                        | X86Register::MtrrPhysBase4(_)
                        | X86Register::MtrrPhysMask4(_)
                        | X86Register::MtrrFix4kE0000(_)
                        | X86Register::MtrrFix4kE8000(_)
                        | X86Register::MtrrFix4kF0000(_)
                        | X86Register::MtrrFix4kF8000(_) => reg,
                    };

                    loader
                        .import_vp_register(reloc_reg)
                        .map_err(Error::Loader)?;
                }

                if relocations_enabled {
                    // Cr3 and Cr4 must be set, as both are used to reconstruct
                    // the page table. This is an invalid igvm file otherwise.
                    match (cr3, cr4) {
                        (Some(cr3), Some(cr4)) => {
                            if vtl
                                == page_table_fixup
                                    .as_ref()
                                    .expect("relocations enabled must be set")
                                    .vtl
                            {
                                page_table_cpu_state = Some(CpuPagingState { cr3, cr4 })
                            }
                        }
                        _ => panic!("invalid igvm file"),
                    }
                }
            }
            IgvmDirectiveHeader::AArch64VbsVpContext { .. } => {
                todo!("AArch64 VP context not supported")
            }
            IgvmDirectiveHeader::ParameterInsert(IGVM_VHS_PARAMETER_INSERT {
                gpa,
                compatibility_mask: _,
                parameter_area_index,
            }) => {
                // Preserve order of import page calls.
                page_data.flush(&mut loader)?;
                let gpa = relocate_gpa(gpa);

                debug_assert!(gpa % HV_PAGE_SIZE == 0);

                let area = parameter_areas
                    .get_mut(&parameter_area_index)
                    .expect("igvmfile should be valid");
                match std::mem::replace(area, ParameterAreaState::Inserted) {
                    ParameterAreaState::Allocated { data, max_size } => loader
                        .import_pages(
                            gpa / HV_PAGE_SIZE,
                            max_size / HV_PAGE_SIZE,
                            "igvm-parameter",
                            BootPageAcceptance::ExclusiveUnmeasured,
                            &data,
                        )
                        .map_err(Error::Loader)?,
                    ParameterAreaState::Inserted => panic!("igvmfile is invalid, multiple insert"),
                }
            }
            IgvmDirectiveHeader::ErrorRange { .. } => {
                todo!("Error Range not supported")
            }
            IgvmDirectiveHeader::X64NativeVpContext { .. } => {
                todo!("native vp context not supported")
            }
        }
    }

    page_data.flush(&mut loader)?;

    // Apply page table relocations after all headers have been scanned.
    if let Some(offset) = relocation_offset {
        // Fixup the page table, the same relocation offset is applied.
        let page_table_cpu_state = page_table_cpu_state
            .expect("igvm file should be valid and vp context should be present");
        let page_table_fixup = page_table_fixup.take().expect("should be some");
        let relocation_region = relocation_region.as_ref().expect("should be some");

        let reloc_region_base_gpa = page_table_fixup.gpa + offset;
        let mut reloc_regions = RangeMap::new();
        reloc_regions.insert(
            relocation_region.base_gpa..=relocation_region.base_gpa + relocation_region.size - 1,
            offset as i64,
        );
        let page_table = page_table_fixup
            .build(offset as i64, reloc_regions, page_table_cpu_state)
            .map_err(Error::PageTableBuilder)?;

        loader
            .import_pages(
                reloc_region_base_gpa / HV_PAGE_SIZE,
                page_table.len() as u64 / HV_PAGE_SIZE,
                "igvm-page-table",
                BootPageAcceptance::Exclusive,
                &page_table,
            )
            .map_err(Error::Loader)?;
    }

    Ok(loader.initial_regs_and_accepted_ranges())
}

/// Build the IGVM memory map reported to the guest, with the specified memory
/// layout and VTL2 ram range. Carry NUMA node information on the side for
/// callers who want it.
fn build_memory_map(
    all_ram: &[MemoryRangeWithNode],
    vtl2_protectable_ram: &[MemoryRange],
) -> (Vec<IGVM_VHS_MEMORY_MAP_ENTRY>, Vec<u32>) {
    let mut memory_map = Vec::new();
    let mut vnodes = Vec::new();

    for (range, r) in memory_range::walk_ranges(
        all_ram.iter().map(|r| (r.range, r.vnode)),
        memory_range::flatten_ranges(vtl2_protectable_ram.iter().copied()).map(|r| (r, ())),
    ) {
        match r {
            memory_range::RangeWalkResult::Neither => {}
            memory_range::RangeWalkResult::Left(vnode) => {
                memory_map.push(memory_map_entry(&range));
                vnodes.push(vnode);
            }
            memory_range::RangeWalkResult::Right(()) => {
                unreachable!("vtl2 protectable range not in all RAM")
            }
            memory_range::RangeWalkResult::Both(vnode, ()) => {
                memory_map.push(IGVM_VHS_MEMORY_MAP_ENTRY {
                    starting_gpa_page_number: range.start_4k_gpn(),
                    number_of_pages: range.page_count_4k(),
                    entry_type: igvm_defs::MemoryMapEntryType::VTL2_PROTECTABLE,
                    flags: 0,
                    reserved: 0,
                });
                vnodes.push(vnode);
            }
        }
    }

    assert_eq!(memory_map.len(), vnodes.len());
    (memory_map, vnodes)
}

#[cfg_attr(not(guest_arch = "aarch64"), expect(dead_code))]
fn load_igvm_aarch64(
    _params: LoadIgvmParams<'_, Aarch64Topology>,
) -> Result<(Vec<Aarch64Register>, Vec<(MemoryRange, PageVisibility)>), Error> {
    Err(Error::UnsupportedGuestArch)
}

// Used to reduce calls into `import_pages`.
//
// FUTURE: just do this optimization in the IGVM file parser to avoid needing to
// reallocate the buffer.
struct PageDataBuffer {
    gpa: u64,
    acceptance: BootPageAcceptance,
    len: u64,
    data: Vec<u8>,
}

impl PageDataBuffer {
    fn new() -> Self {
        Self {
            gpa: 0,
            acceptance: BootPageAcceptance::Exclusive,
            len: 0,
            data: Vec::new(),
        }
    }

    fn append<R: GuestArch>(
        &mut self,
        loader: &mut dyn ImageLoad<R>,
        gpa: u64,
        acceptance: BootPageAcceptance,
        data: &[u8],
    ) -> Result<(), Error> {
        // Only full 4K pages supported right now. No reason to support
        // truncated pages, and supporting 2M pages will require changes to the
        // trait to tell the loader to measure in 2M chunks (for CVM).
        assert_eq!(data.len() as u64, HV_PAGE_SIZE);

        // Flush if this is non-contiguous or has a different acceptance type,
        // or if there is unbuffered trailing zero data.
        if self.len == 0
            || (self.data.len() as u64) < self.len
            || self.gpa + self.len != gpa
            || self.acceptance != acceptance
        {
            self.flush(loader)?;
            self.gpa = gpa;
            self.acceptance = acceptance;
        }
        self.data.extend_from_slice(data);
        self.len += data.len() as u64;
        Ok(())
    }

    fn zero<R: GuestArch>(
        &mut self,
        loader: &mut dyn ImageLoad<R>,
        gpa: u64,
        acceptance: BootPageAcceptance,
        len: u64,
    ) -> Result<(), Error> {
        // Same comment in `append` applies here.
        assert_eq!(len, HV_PAGE_SIZE);

        // Flush if this is non-contiguous or has a different acceptance type.
        if self.len == 0 || self.gpa + self.len != gpa || self.acceptance != acceptance {
            self.flush(loader)?;
            self.gpa = gpa;
            self.acceptance = acceptance;
        }
        self.len += len;
        Ok(())
    }

    fn flush<R: GuestArch>(&mut self, loader: &mut dyn ImageLoad<R>) -> Result<(), Error> {
        if self.len == 0 {
            assert!(self.data.is_empty());
            return Ok(());
        }
        loader
            .import_pages(
                self.gpa / HV_PAGE_SIZE,
                self.len / HV_PAGE_SIZE,
                "igvm-data",
                self.acceptance,
                &self.data,
            )
            .map_err(Error::Loader)?;

        self.data.clear();
        self.len = 0;
        Ok(())
    }
}
