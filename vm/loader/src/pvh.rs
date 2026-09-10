// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Xen PVH direct-boot loader for x86-64 guests.

use crate::common::ChunkBuf;
use crate::common::ImportFileRegion;
use crate::common::ImportFileRegionError;
use crate::importer::BootPageAcceptance;
use crate::importer::ImageLoad;
use crate::importer::SegmentRegister;
use crate::importer::StartupMemoryType;
use crate::importer::TableRegister;
use crate::importer::X86Register;
use hvdef::HV_PAGE_SIZE;
use memory_range::MemoryRange;
use object::LittleEndian;
use object::ReadCache;
use object::ReadRef;
use object::elf;
use object::read::elf::FileHeader;
use std::io::Read;
use std::io::Seek;
use thiserror::Error;
use vm_topology::memory::MemoryLayout;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const LE: LittleEndian = LittleEndian {};
const FOUR_GB: u64 = 0x1_0000_0000;
const HIMEM_START: u64 = 0x10_0000;
const MP_FLOATING_POINTER_ADDR: usize = 0;
const MP_CONFIG_TABLE_ADDR: usize = 0x400;
const MP_IRQ_FLAGS_LEVEL_HIGH: u16 = 0x000d;
const BOOT_GDT_ADDR: u64 = 0x800;
const START_INFO_ADDR: u64 = 0x6000;
const MODLIST_ADDR: u64 = 0x6040;
const MEMMAP_ADDR: u64 = 0x7000;
/// Fixed RSDP address in the PVH boot metadata region.
pub const ACPI_RSDP_ADDR: u64 = 0x8000;
const ACPI_TABLES_ADDR: u64 = ACPI_RSDP_ADDR + HV_PAGE_SIZE;
const CMDLINE_ADDR: u64 = 0x2_0000;
const CMDLINE_MAX_SIZE: usize = 64 * 1024;
const XEN_ELFNOTE_PHYS32_ENTRY: u32 = 18;
const XEN_HVM_START_MAGIC_VALUE: u32 = 0x336e_c578;
const XEN_HVM_MEMMAP_TYPE_RAM: u32 = 1;
const XEN_HVM_MEMMAP_TYPE_RESERVED: u32 = 2;
const MAX_NOTE_SIZE: u64 = 1024 * 1024;

const SEG_ATTR_CODE: u16 = 0xc09b;
const SEG_ATTR_DATA: u16 = 0xc093;
const SEG_ATTR_TSS: u16 = 0x008b;

#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout)]
struct HvmStartInfo {
    magic: u32,
    version: u32,
    flags: u32,
    nr_modules: u32,
    modlist_paddr: u64,
    cmdline_paddr: u64,
    rsdp_paddr: u64,
    memmap_paddr: u64,
    memmap_entries: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout)]
struct HvmModlistEntry {
    paddr: u64,
    size: u64,
    cmdline_paddr: u64,
    reserved: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, IntoBytes, Immutable, KnownLayout)]
struct HvmMemmapTableEntry {
    addr: u64,
    size: u64,
    entry_type: u32,
    reserved: u32,
}

/// Optional initramfs input.
pub struct InitrdConfig<'a, R: Read + Seek> {
    /// Initramfs reader.
    pub image: &'a mut R,
    /// Initramfs size in bytes.
    pub size: u64,
}

/// ACPI tables to expose through Xen PVH start info.
#[derive(Debug)]
pub struct AcpiTables {
    /// The RSDP, which must fit in one page.
    pub rsdp: Vec<u8>,
    /// The tables referenced by the RSDP.
    pub tables: Vec<u8>,
}

/// Guest-visible processor and interrupt data for Xen PVH boot tables.
#[derive(Debug, Clone, Copy)]
pub struct BootConfig<'a> {
    /// APIC IDs in virtual-processor order. The first entry is the BSP.
    pub apic_ids: &'a [u32],
    /// ISA IRQs described as active-high, level-triggered.
    pub level_triggered_irqs: &'a [u32],
    /// Page-aligned RAM ranges published as reserved in the PVH memory map.
    pub reserved_memory_ranges: &'a [MemoryRange],
}

/// Guest placement selected by the loader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadInfo {
    /// Xen physical entry address.
    pub entrypoint: u64,
    /// Optional initramfs guest-physical range `(base, size)`.
    pub initrd: Option<(u64, u64)>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to access the kernel image")]
    KernelIo(#[source] std::io::Error),
    #[error("failed to read the ELF64 header")]
    ReadFileHeader,
    #[error("invalid ELF64 header")]
    InvalidFileHeader,
    #[error("PVH kernel is not little-endian")]
    BigEndian,
    #[error("PVH kernel is not an x86-64 ELF image")]
    WrongMachine,
    #[error("failed to parse ELF program headers")]
    InvalidProgramHeaders(#[source] object::read::Error),
    #[error("ELF program header arithmetic overflowed")]
    ProgramHeaderOverflow,
    #[error("ELF segment file size exceeds its memory size")]
    FileSizeExceedsMemorySize,
    #[error("ELF segment lies outside the kernel image")]
    SegmentOutsideFile,
    #[error("ELF load segment is empty")]
    EmptyLoadSegment,
    #[error("ELF load segment at {start:#x}..{end:#x} is below 1 MiB")]
    SegmentBelowOneMb { start: u64, end: u64 },
    #[error("ELF load segments overlap at page granularity")]
    OverlappingLoadSegments,
    #[error("ELF note segment exceeds the 1-MiB parser bound")]
    NoteTooLarge,
    #[error("malformed ELF note")]
    MalformedNote,
    #[error("multiple Xen PVH entry notes are present")]
    DuplicatePvhEntry,
    #[error("kernel does not contain XEN_ELFNOTE_PHYS32_ENTRY")]
    MissingPvhEntry,
    #[error("Xen PVH entry address does not fit in 32 bits")]
    EntryAboveFourGb,
    #[error("Xen PVH entry address is not within a load segment")]
    EntryOutsideLoadSegment,
    #[error("kernel command line contains an embedded NUL")]
    CommandLineNul,
    #[error("kernel command line exceeds the 64-KiB PVH limit")]
    CommandLineTooLong,
    #[error("PVH memory map does not fit in its reserved page")]
    MemoryMapTooLarge,
    #[error("invalid PVH reserved memory range {start:#x}..{end:#x}")]
    InvalidReservedMemoryRange { start: u64, end: u64 },
    #[error("PVH ACPI data does not fit in its reserved boot metadata region")]
    AcpiTablesTooLarge,
    #[error("PVH processor topology must contain at least one processor")]
    NoProcessors,
    #[error("PVH processor {index} has APIC ID {apic_id}; expected {expected}")]
    InvalidApicId {
        index: usize,
        apic_id: u32,
        expected: u32,
    },
    #[error("PVH MP table has too many processor or interrupt entries")]
    TooManyMpEntries,
    #[error("PVH MP table IRQ {0} is outside the ISA range")]
    InvalidMpIrq(u32),
    #[error("PVH MP table ending at {table_end:#x} overlaps the GDT at {gdt_addr:#x}")]
    MpTableOverlap { table_end: usize, gdt_addr: u64 },
    #[error("initramfs is empty")]
    EmptyInitrd,
    #[error("initramfs does not fit above the kernel in low RAM")]
    InitrdDoesNotFit,
    #[error("guest address computation overflowed")]
    AddressOverflow,
    #[error("required guest RAM is unavailable for {tag}")]
    VerifyMemory {
        tag: &'static str,
        #[source]
        source: anyhow::Error,
    },
    #[error("failed to import {tag}")]
    ImportPages {
        tag: &'static str,
        #[source]
        source: anyhow::Error,
    },
    #[error("failed to import ELF or initramfs data")]
    ImportFileRegion(#[source] ImportFileRegionError),
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    file_offset: u64,
    file_size: u64,
    gpa: u64,
    memory_size: u64,
}

impl Segment {
    fn end(self) -> Result<u64, Error> {
        self.gpa
            .checked_add(self.memory_size)
            .ok_or(Error::AddressOverflow)
    }

    fn page_span(self) -> Result<(u64, u64), Error> {
        page_span(self.gpa, self.memory_size)
    }
}

struct ParsedKernel {
    segments: Vec<Segment>,
    entrypoint: u64,
}

/// Loads an x86-64 Xen PVH ELF image with explicit boot-table configuration.
pub fn load_with_boot_config<F, R>(
    importer: &mut dyn ImageLoad<X86Register>,
    kernel: &mut F,
    initrd: Option<InitrdConfig<'_, R>>,
    cmdline: &str,
    memory_layout: &MemoryLayout,
    acpi_tables: Option<&AcpiTables>,
    boot_config: &BootConfig<'_>,
) -> Result<LoadInfo, Error>
where
    F: Read + Seek,
    R: Read + Seek,
{
    if cmdline.contains('\0') {
        return Err(Error::CommandLineNul);
    }
    let cmdline_size = cmdline.len().checked_add(1).ok_or(Error::AddressOverflow)?;
    if cmdline_size > CMDLINE_MAX_SIZE {
        return Err(Error::CommandLineTooLong);
    }

    let ParsedKernel {
        segments,
        entrypoint,
    } = parse_kernel(kernel)?;

    let mut chunk = ChunkBuf::new();
    for segment in &segments {
        let (page_base, page_count) = segment.page_span()?;
        verify_memory(importer, page_base, page_count, "pvh-kernel")?;
        chunk
            .import_file_region(
                importer,
                ImportFileRegion {
                    file: kernel,
                    file_offset: segment.file_offset,
                    file_length: segment.file_size,
                    gpa: segment.gpa,
                    memory_length: segment.memory_size,
                    acceptance: BootPageAcceptance::Exclusive,
                    tag: "pvh-kernel",
                },
            )
            .map_err(Error::ImportFileRegion)?;
    }

    let initrd = match initrd {
        Some(initrd) => {
            if initrd.size == 0 {
                return Err(Error::EmptyInitrd);
            }
            let base = place_initrd(memory_layout, initrd.size, &segments)?;

            let (page_base, page_count) = page_span(base, initrd.size)?;
            verify_memory(importer, page_base, page_count, "pvh-initrd")?;
            chunk
                .import_file_region(
                    importer,
                    ImportFileRegion {
                        file: initrd.image,
                        file_offset: 0,
                        file_length: initrd.size,
                        gpa: base,
                        memory_length: initrd.size,
                        acceptance: BootPageAcceptance::Exclusive,
                        tag: "pvh-initrd",
                    },
                )
                .map_err(Error::ImportFileRegion)?;
            Some((base, initrd.size))
        }
        None => None,
    };

    import_boot_structures(
        importer,
        cmdline,
        memory_layout,
        initrd,
        acpi_tables,
        boot_config,
    )?;
    import_registers(importer, entrypoint)?;

    Ok(LoadInfo { entrypoint, initrd })
}

fn place_initrd(
    memory_layout: &MemoryLayout,
    size: u64,
    segments: &[Segment],
) -> Result<u64, Error> {
    let low_range = memory_layout
        .ram()
        .iter()
        .filter(|range| range.range.start() < FOUR_GB)
        .max_by_key(|range| range.range.end().min(FOUR_GB))
        .ok_or(Error::InitrdDoesNotFit)?;
    let low_end = low_range.range.end().min(FOUR_GB);
    let unaligned_base = low_end.checked_sub(size).ok_or(Error::InitrdDoesNotFit)?;
    let base = unaligned_base & !(HV_PAGE_SIZE - 1);
    if base < HIMEM_START || base < low_range.range.start() {
        return Err(Error::InitrdDoesNotFit);
    }

    let (initrd_page_base, initrd_page_count) = page_span(base, size)?;
    let initrd_page_end = initrd_page_base
        .checked_add(initrd_page_count)
        .ok_or(Error::AddressOverflow)?;
    for segment in segments {
        let (segment_page_base, segment_page_count) = segment.page_span()?;
        let segment_page_end = segment_page_base
            .checked_add(segment_page_count)
            .ok_or(Error::AddressOverflow)?;
        if initrd_page_base < segment_page_end && segment_page_base < initrd_page_end {
            return Err(Error::InitrdDoesNotFit);
        }
    }
    Ok(base)
}

fn parse_kernel<F: Read + Seek>(kernel: &mut F) -> Result<ParsedKernel, Error> {
    let image_size = kernel
        .seek(std::io::SeekFrom::End(0))
        .map_err(Error::KernelIo)?;
    kernel.rewind().map_err(Error::KernelIo)?;

    let reader = ReadCache::new(&mut *kernel);
    let header: &elf::FileHeader64<LittleEndian> =
        reader.read_at(0).map_err(|_| Error::ReadFileHeader)?;
    if !header.is_supported() {
        return Err(Error::InvalidFileHeader);
    }
    if header.is_big_endian() {
        return Err(Error::BigEndian);
    }
    if header.e_machine.get(LE) != elf::EM_X86_64 {
        return Err(Error::WrongMachine);
    }
    let program_headers = header
        .program_headers(LE, &reader)
        .map_err(Error::InvalidProgramHeaders)?;

    let mut segments = Vec::new();
    let mut note_regions = Vec::new();
    for program_header in program_headers {
        let file_offset = program_header.p_offset.get(LE);
        let segment_file_size = program_header.p_filesz.get(LE);
        let file_end = file_offset
            .checked_add(segment_file_size)
            .ok_or(Error::ProgramHeaderOverflow)?;
        if file_end > image_size {
            return Err(Error::SegmentOutsideFile);
        }

        match program_header.p_type.get(LE) {
            elf::PT_LOAD => {
                let memory_size = program_header.p_memsz.get(LE);
                if segment_file_size > memory_size {
                    return Err(Error::FileSizeExceedsMemorySize);
                }
                if memory_size == 0 {
                    return Err(Error::EmptyLoadSegment);
                }
                let gpa = program_header.p_paddr.get(LE);
                let end = gpa
                    .checked_add(memory_size)
                    .ok_or(Error::ProgramHeaderOverflow)?;
                if gpa < HIMEM_START {
                    return Err(Error::SegmentBelowOneMb { start: gpa, end });
                }
                segments.push(Segment {
                    file_offset,
                    file_size: segment_file_size,
                    gpa,
                    memory_size,
                });
            }
            elf::PT_NOTE => {
                if segment_file_size > MAX_NOTE_SIZE {
                    return Err(Error::NoteTooLarge);
                }
                note_regions.push((file_offset, segment_file_size));
            }
            _ => {}
        }
    }
    drop(reader);

    segments.sort_by_key(|segment| segment.gpa);
    if segments.is_empty() {
        return Err(Error::MissingPvhEntry);
    }
    for pair in segments.windows(2) {
        let (left_base, left_count) = pair[0].page_span()?;
        let (right_base, _) = pair[1].page_span()?;
        let left_end = left_base
            .checked_add(left_count)
            .ok_or(Error::AddressOverflow)?;
        if right_base < left_end {
            return Err(Error::OverlappingLoadSegments);
        }
    }

    let mut entrypoint = None;
    for (offset, size) in note_regions {
        let size = usize::try_from(size).map_err(|_| Error::NoteTooLarge)?;
        let mut notes = vec![0; size];
        kernel
            .seek(std::io::SeekFrom::Start(offset))
            .map_err(Error::KernelIo)?;
        kernel.read_exact(&mut notes).map_err(Error::KernelIo)?;
        if let Some(entry) = find_pvh_entry(&notes)? {
            if entrypoint.replace(entry).is_some() {
                return Err(Error::DuplicatePvhEntry);
            }
        }
    }
    let entrypoint = entrypoint.ok_or(Error::MissingPvhEntry)?;
    if entrypoint > u32::MAX as u64 {
        return Err(Error::EntryAboveFourGb);
    }
    let mut entry_in_segment = false;
    for segment in &segments {
        if entrypoint >= segment.gpa && entrypoint < segment.end()? {
            entry_in_segment = true;
            break;
        }
    }
    if !entry_in_segment {
        return Err(Error::EntryOutsideLoadSegment);
    }

    Ok(ParsedKernel {
        segments,
        entrypoint,
    })
}

fn find_pvh_entry(notes: &[u8]) -> Result<Option<u64>, Error> {
    let mut offset = 0usize;
    let mut entrypoint = None;
    while offset < notes.len() {
        let header_end = offset.checked_add(12).ok_or(Error::MalformedNote)?;
        let header = notes.get(offset..header_end).ok_or(Error::MalformedNote)?;
        let name_size = read_note_u32(header, 0)? as usize;
        let descriptor_size = read_note_u32(header, 4)? as usize;
        let note_type = read_note_u32(header, 8)?;
        let name_start = header_end;
        let name_end = name_start
            .checked_add(name_size)
            .ok_or(Error::MalformedNote)?;
        let descriptor_start = align_up_usize(name_end, 4).ok_or(Error::MalformedNote)?;
        let descriptor_end = descriptor_start
            .checked_add(descriptor_size)
            .ok_or(Error::MalformedNote)?;
        let next = align_up_usize(descriptor_end, 4).ok_or(Error::MalformedNote)?;
        let name = notes
            .get(name_start..name_end)
            .ok_or(Error::MalformedNote)?;
        let descriptor = notes
            .get(descriptor_start..descriptor_end)
            .ok_or(Error::MalformedNote)?;

        if note_type == XEN_ELFNOTE_PHYS32_ENTRY && name.starts_with(b"Xen") {
            let entry = match descriptor {
                [a, b, c, d] => u32::from_le_bytes([*a, *b, *c, *d]) as u64,
                [a, b, c, d, e, f, g, h] => u64::from_le_bytes([*a, *b, *c, *d, *e, *f, *g, *h]),
                _ => return Err(Error::MalformedNote),
            };
            if entrypoint.replace(entry).is_some() {
                return Err(Error::DuplicatePvhEntry);
            }
        }

        offset = next;
    }
    Ok(entrypoint)
}

fn read_note_u32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let end = offset.checked_add(4).ok_or(Error::MalformedNote)?;
    let bytes = bytes.get(offset..end).ok_or(Error::MalformedNote)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn import_boot_structures(
    importer: &mut dyn ImageLoad<X86Register>,
    cmdline: &str,
    memory_layout: &MemoryLayout,
    initrd: Option<(u64, u64)>,
    acpi_tables: Option<&AcpiTables>,
    boot_config: &BootConfig<'_>,
) -> Result<(), Error> {
    let mut boot_page = [0u8; HV_PAGE_SIZE as usize];
    write_mp_tables(&mut boot_page, boot_config)?;
    let boot_gdt_addr = BOOT_GDT_ADDR;
    let gdt = [
        0,
        gdt_entry(SEG_ATTR_CODE, 0, 0x000f_ffff),
        gdt_entry(SEG_ATTR_DATA, 0, 0x000f_ffff),
        gdt_entry(SEG_ATTR_TSS, 0, 0x67),
    ];
    for (index, entry) in gdt.into_iter().enumerate() {
        let offset = boot_gdt_addr as usize + index * size_of::<u64>();
        boot_page[offset..offset + size_of::<u64>()].copy_from_slice(&entry.to_le_bytes());
    }
    import_pages(importer, 0, 1, "pvh-boot-tables", &boot_page)?;

    let memory_ranges = pvh_memory_map(memory_layout, boot_config.reserved_memory_ranges)?;
    let memmap_size = memory_ranges
        .len()
        .checked_mul(size_of::<HvmMemmapTableEntry>())
        .ok_or(Error::AddressOverflow)?;
    if memmap_size > HV_PAGE_SIZE as usize {
        return Err(Error::MemoryMapTooLarge);
    }
    let memmap_entries =
        u32::try_from(memory_ranges.len()).map_err(|_| Error::MemoryMapTooLarge)?;
    let mut memmap_page = [0u8; HV_PAGE_SIZE as usize];
    for (index, entry) in memory_ranges.iter().enumerate() {
        let offset = index * size_of::<HvmMemmapTableEntry>();
        memmap_page[offset..offset + size_of::<HvmMemmapTableEntry>()]
            .copy_from_slice(entry.as_bytes());
    }
    import_pages(
        importer,
        MEMMAP_ADDR / HV_PAGE_SIZE,
        1,
        "pvh-memory-map",
        &memmap_page,
    )?;

    let mut start_page = [0u8; HV_PAGE_SIZE as usize];
    if let Some((base, size)) = initrd {
        let module = HvmModlistEntry {
            paddr: base,
            size,
            cmdline_paddr: 0,
            reserved: 0,
        };
        let offset = (MODLIST_ADDR - START_INFO_ADDR) as usize;
        start_page[offset..offset + size_of::<HvmModlistEntry>()]
            .copy_from_slice(module.as_bytes());
    }
    let start_info = HvmStartInfo {
        magic: XEN_HVM_START_MAGIC_VALUE,
        version: 1,
        flags: 0,
        nr_modules: u32::from(initrd.is_some()),
        modlist_paddr: if initrd.is_some() { MODLIST_ADDR } else { 0 },
        cmdline_paddr: CMDLINE_ADDR,
        rsdp_paddr: if acpi_tables.is_some() {
            ACPI_RSDP_ADDR
        } else {
            0
        },
        memmap_paddr: MEMMAP_ADDR,
        memmap_entries,
        reserved: 0,
    };
    start_page[..size_of::<HvmStartInfo>()].copy_from_slice(start_info.as_bytes());
    import_pages(
        importer,
        START_INFO_ADDR / HV_PAGE_SIZE,
        1,
        "pvh-start-info",
        &start_page,
    )?;

    if let Some(acpi_tables) = acpi_tables {
        if acpi_tables.rsdp.len() > HV_PAGE_SIZE as usize
            || ACPI_TABLES_ADDR
                .checked_add(acpi_tables.tables.len() as u64)
                .is_none_or(|end| end > CMDLINE_ADDR)
        {
            return Err(Error::AcpiTablesTooLarge);
        }

        let mut rsdp_page = [0; HV_PAGE_SIZE as usize];
        rsdp_page[..acpi_tables.rsdp.len()].copy_from_slice(&acpi_tables.rsdp);
        import_pages(
            importer,
            ACPI_RSDP_ADDR / HV_PAGE_SIZE,
            1,
            "pvh-acpi-rsdp",
            &rsdp_page,
        )?;

        let table_pages = (acpi_tables.tables.len() as u64).div_ceil(HV_PAGE_SIZE);
        let mut table_data = vec![0; (table_pages * HV_PAGE_SIZE) as usize];
        table_data[..acpi_tables.tables.len()].copy_from_slice(&acpi_tables.tables);
        import_pages(
            importer,
            ACPI_TABLES_ADDR / HV_PAGE_SIZE,
            table_pages,
            "pvh-acpi-tables",
            &table_data,
        )?;
    }

    let cmdline_size = cmdline.len().checked_add(1).ok_or(Error::AddressOverflow)?;
    let cmdline_pages = (cmdline_size as u64).div_ceil(HV_PAGE_SIZE);
    let mut cmdline_data = vec![0; (cmdline_pages * HV_PAGE_SIZE) as usize];
    cmdline_data[..cmdline.len()].copy_from_slice(cmdline.as_bytes());
    import_pages(
        importer,
        CMDLINE_ADDR / HV_PAGE_SIZE,
        cmdline_pages,
        "pvh-command-line",
        &cmdline_data,
    )?;

    Ok(())
}

fn pvh_memory_map(
    memory_layout: &MemoryLayout,
    reserved_ranges: &[MemoryRange],
) -> Result<Vec<HvmMemmapTableEntry>, Error> {
    let mut previous_end = 0;
    for range in reserved_ranges {
        let valid = !range.is_empty()
            && range.start().is_multiple_of(HV_PAGE_SIZE)
            && range.end().is_multiple_of(HV_PAGE_SIZE)
            && range.start() >= previous_end
            && memory_layout
                .ram()
                .iter()
                .any(|ram| range.start() >= ram.range.start() && range.end() <= ram.range.end());
        if !valid {
            return Err(Error::InvalidReservedMemoryRange {
                start: range.start(),
                end: range.end(),
            });
        }
        previous_end = range.end();
    }

    let mut entries = Vec::with_capacity(memory_layout.ram().len() + reserved_ranges.len() * 2);
    for ram in memory_layout.ram() {
        let mut next = ram.range.start();
        for reserved in reserved_ranges {
            if reserved.start() < ram.range.start() || reserved.end() > ram.range.end() {
                continue;
            }
            if next < reserved.start() {
                entries.push(HvmMemmapTableEntry {
                    addr: next,
                    size: reserved.start() - next,
                    entry_type: XEN_HVM_MEMMAP_TYPE_RAM,
                    reserved: 0,
                });
            }
            entries.push(HvmMemmapTableEntry {
                addr: reserved.start(),
                size: reserved.len(),
                entry_type: XEN_HVM_MEMMAP_TYPE_RESERVED,
                reserved: 0,
            });
            next = reserved.end();
        }
        if next < ram.range.end() {
            entries.push(HvmMemmapTableEntry {
                addr: next,
                size: ram.range.end() - next,
                entry_type: XEN_HVM_MEMMAP_TYPE_RAM,
                reserved: 0,
            });
        }
    }
    Ok(entries)
}

fn write_mp_tables(
    page: &mut [u8; HV_PAGE_SIZE as usize],
    boot_config: &BootConfig<'_>,
) -> Result<(), Error> {
    let table = build_mp_config_table(boot_config)?;
    let table_end = MP_CONFIG_TABLE_ADDR + table.len();
    let gdt_addr = BOOT_GDT_ADDR;
    if table_end > gdt_addr as usize {
        return Err(Error::MpTableOverlap {
            table_end,
            gdt_addr,
        });
    }
    page[MP_CONFIG_TABLE_ADDR..table_end].copy_from_slice(&table);

    let mut floating_pointer = [0u8; 16];
    floating_pointer[..4].copy_from_slice(b"_MP_");
    floating_pointer[4..8].copy_from_slice(&(MP_CONFIG_TABLE_ADDR as u32).to_le_bytes());
    floating_pointer[8] = 1;
    floating_pointer[9] = 4;
    floating_pointer[10] = checksum(&floating_pointer);
    page[MP_FLOATING_POINTER_ADDR..MP_FLOATING_POINTER_ADDR + floating_pointer.len()]
        .copy_from_slice(&floating_pointer);
    Ok(())
}

/// Builds the Intel MP configuration table for the selected PVH boot contract.
pub fn build_mp_config_table(boot_config: &BootConfig<'_>) -> Result<Vec<u8>, Error> {
    const MP_CONFIG_HEADER_SIZE: usize = 44;
    const MP_PROCESSOR_SIZE: usize = 20;
    const MP_NON_PROCESSOR_ENTRY_COUNT: usize = 17;

    if boot_config.apic_ids.is_empty() {
        return Err(Error::NoProcessors);
    }
    if let Some(&irq) = boot_config
        .level_triggered_irqs
        .iter()
        .find(|irq| **irq >= 16)
    {
        return Err(Error::InvalidMpIrq(irq));
    }
    let entry_count = u16::try_from(
        boot_config
            .apic_ids
            .len()
            .checked_add(MP_NON_PROCESSOR_ENTRY_COUNT)
            .ok_or(Error::TooManyMpEntries)?,
    )
    .map_err(|_| Error::TooManyMpEntries)?;

    let mut table = Vec::with_capacity(180 + MP_PROCESSOR_SIZE * boot_config.apic_ids.len());
    table.extend_from_slice(b"PCMP");
    table.extend_from_slice(&0u16.to_le_bytes());
    table.push(4);
    table.push(0);
    table.extend_from_slice(b"OPENVMM ");
    table.extend_from_slice(b"MICROVM     ");
    table.extend_from_slice(&0u32.to_le_bytes());
    table.extend_from_slice(&0u16.to_le_bytes());
    table.extend_from_slice(&entry_count.to_le_bytes());
    table.extend_from_slice(&0xfee0_0000u32.to_le_bytes());
    table.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(table.len(), MP_CONFIG_HEADER_SIZE);

    for (index, &apic_id) in boot_config.apic_ids.iter().enumerate() {
        let expected = u32::try_from(index).map_err(|_| Error::TooManyMpEntries)?;
        if apic_id != expected {
            return Err(Error::InvalidApicId {
                index,
                apic_id,
                expected,
            });
        }
        let apic_id = u8::try_from(apic_id).map_err(|_| Error::TooManyMpEntries)?;
        table.extend_from_slice(&[0, apic_id, 0x14, if index == 0 { 3 } else { 1 }]);
        table.extend_from_slice(&0u32.to_le_bytes());
        table.extend_from_slice(&0u32.to_le_bytes());
        table.extend_from_slice(&[0; 8]);
    }
    assert_eq!(
        table.len(),
        MP_CONFIG_HEADER_SIZE + MP_PROCESSOR_SIZE * boot_config.apic_ids.len()
    );

    table.extend_from_slice(&[1, 0]);
    table.extend_from_slice(b"ISA   ");
    table.extend_from_slice(&[2, 0, 0x11, 1]);
    table.extend_from_slice(&0xfec0_0000u32.to_le_bytes());

    for irq in (0u8..16).filter(|irq| *irq != 2) {
        let pin = if irq == 0 { 2 } else { irq };
        let flags = if boot_config.level_triggered_irqs.contains(&u32::from(irq)) {
            MP_IRQ_FLAGS_LEVEL_HIGH
        } else {
            0
        };
        table.extend_from_slice(&[3, 0]);
        table.extend_from_slice(&flags.to_le_bytes());
        table.extend_from_slice(&[0, irq, 0, pin]);
    }

    let table_len = u16::try_from(table.len()).expect("MP table fits in one page");
    table[4..6].copy_from_slice(&table_len.to_le_bytes());
    table[7] = checksum(&table);
    Ok(table)
}

fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(
        bytes
            .iter()
            .copied()
            .fold(0u8, |sum, byte| sum.wrapping_add(byte)),
    )
}

fn import_registers(
    importer: &mut dyn ImageLoad<X86Register>,
    entrypoint: u64,
) -> Result<(), Error> {
    let boot_gdt_addr = BOOT_GDT_ADDR;
    let data_segment = SegmentRegister {
        base: 0,
        limit: u32::MAX,
        selector: 0x10,
        attributes: SEG_ATTR_DATA,
    };
    let code_segment = SegmentRegister {
        base: 0,
        limit: u32::MAX,
        selector: 0x08,
        attributes: SEG_ATTR_CODE,
    };
    let registers = [
        X86Register::Gdtr(TableRegister {
            base: boot_gdt_addr,
            limit: 31,
        }),
        X86Register::Idtr(TableRegister {
            base: boot_gdt_addr + 0x20,
            limit: 0,
        }),
        X86Register::Ds(data_segment),
        X86Register::Es(data_segment),
        X86Register::Fs(data_segment),
        X86Register::Gs(data_segment),
        X86Register::Ss(data_segment),
        X86Register::Cs(code_segment),
        X86Register::Tr(SegmentRegister {
            base: 0,
            limit: 0x67,
            selector: 0x18,
            attributes: SEG_ATTR_TSS,
        }),
        X86Register::Cr0(1),
        X86Register::Cr3(0),
        X86Register::Cr4(0),
        X86Register::Efer(0),
        X86Register::Rbx(START_INFO_ADDR),
        X86Register::Rip(entrypoint),
        X86Register::Rsp(0),
        X86Register::Rflags(2),
    ];
    for register in registers {
        importer
            .import_vp_register(register)
            .map_err(|source| Error::ImportPages {
                tag: "pvh-registers",
                source,
            })?;
    }
    Ok(())
}

fn import_pages(
    importer: &mut dyn ImageLoad<X86Register>,
    page_base: u64,
    page_count: u64,
    tag: &'static str,
    data: &[u8],
) -> Result<(), Error> {
    verify_memory(importer, page_base, page_count, tag)?;
    importer
        .import_pages(
            page_base,
            page_count,
            tag,
            BootPageAcceptance::Exclusive,
            data,
        )
        .map_err(|source| Error::ImportPages { tag, source })
}

fn verify_memory(
    importer: &mut dyn ImageLoad<X86Register>,
    page_base: u64,
    page_count: u64,
    tag: &'static str,
) -> Result<(), Error> {
    importer
        .verify_startup_memory_available(page_base, page_count, StartupMemoryType::Ram)
        .map_err(|source| Error::VerifyMemory { tag, source })
}

fn page_span(gpa: u64, size: u64) -> Result<(u64, u64), Error> {
    if size == 0 {
        return Err(Error::EmptyLoadSegment);
    }
    let leading = gpa & (HV_PAGE_SIZE - 1);
    let page_count = leading
        .checked_add(size)
        .and_then(|size| size.checked_add(HV_PAGE_SIZE - 1))
        .ok_or(Error::AddressOverflow)?
        / HV_PAGE_SIZE;
    Ok((gpa / HV_PAGE_SIZE, page_count))
}

fn align_up_usize(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

const fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    (((base as u64) & 0xff00_0000) << 32)
        | (((flags as u64) & 0x0000_f0ff) << 40)
        | (((limit as u64) & 0x000f_0000) << 32)
        | (((base as u64) & 0x00ff_ffff) << 16)
        | ((limit as u64) & 0x0000_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::importer::IgvmParameterType;
    use crate::importer::IsolationConfig;
    use crate::importer::IsolationType;
    use crate::importer::ParameterAreaIndex;
    use memory_range::MemoryRange;
    use std::io::Cursor;

    fn load<F, R>(
        importer: &mut dyn ImageLoad<X86Register>,
        kernel: &mut F,
        initrd: Option<InitrdConfig<'_, R>>,
        cmdline: &str,
        memory_layout: &MemoryLayout,
        acpi_tables: Option<&AcpiTables>,
    ) -> Result<LoadInfo, Error>
    where
        F: Read + Seek,
        R: Read + Seek,
    {
        load_with_boot_config(
            importer,
            kernel,
            initrd,
            cmdline,
            memory_layout,
            acpi_tables,
            &BootConfig {
                apic_ids: &[0],
                level_triggered_irqs: &[],
                reserved_memory_ranges: &[MemoryRange::new(0x3_0000..0x3_1000)],
            },
        )
    }

    #[derive(Default)]
    struct RecordingImporter {
        pages: Vec<(&'static str, u64, u64, Vec<u8>)>,
        registers: Vec<X86Register>,
    }

    impl ImageLoad<X86Register> for RecordingImporter {
        fn isolation_config(&self) -> IsolationConfig {
            IsolationConfig {
                paravisor_present: false,
                isolation_type: IsolationType::None,
                shared_gpa_boundary_bits: None,
            }
        }

        fn create_parameter_area(
            &mut self,
            _: u64,
            _: u32,
            _: &str,
        ) -> anyhow::Result<ParameterAreaIndex> {
            unimplemented!()
        }

        fn create_parameter_area_with_data(
            &mut self,
            _: u64,
            _: u32,
            _: &str,
            _: &[u8],
        ) -> anyhow::Result<ParameterAreaIndex> {
            unimplemented!()
        }

        fn import_parameter(
            &mut self,
            _: ParameterAreaIndex,
            _: u32,
            _: IgvmParameterType,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }

        fn import_pages(
            &mut self,
            page_base: u64,
            page_count: u64,
            tag: &'static str,
            _: BootPageAcceptance,
            data: &[u8],
        ) -> anyhow::Result<()> {
            self.pages.push((tag, page_base, page_count, data.to_vec()));
            Ok(())
        }

        fn import_vp_register(&mut self, register: X86Register) -> anyhow::Result<()> {
            self.registers.push(register);
            Ok(())
        }

        fn verify_startup_memory_available(
            &mut self,
            _: u64,
            _: u64,
            _: StartupMemoryType,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn set_vp_context_page(&mut self, _: u64) -> anyhow::Result<()> {
            unimplemented!()
        }

        fn relocation_region(
            &mut self,
            _: u64,
            _: u64,
            _: u64,
            _: u64,
            _: u64,
            _: bool,
            _: bool,
            _: u16,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }

        fn page_table_relocation(&mut self, _: u64, _: u64, _: u64, _: u16) -> anyhow::Result<()> {
            unimplemented!()
        }

        fn set_imported_regions_config_page(&mut self, _: u64) {
            unimplemented!()
        }
    }

    fn make_layout() -> MemoryLayout {
        MemoryLayout::new(
            64 * 1024 * 1024,
            &[MemoryRange::new(0xc000_0000..FOUR_GB)],
            &[],
            &[],
            None,
        )
        .unwrap()
    }

    fn write_u16(image: &mut [u8], offset: usize, value: u16) {
        image[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(image: &mut [u8], offset: usize, value: u32) {
        image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(image: &mut [u8], offset: usize, value: u64) {
        image[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn test_elf() -> Vec<u8> {
        const NOTE_OFFSET: usize = 0x200;
        const DATA_OFFSET: usize = 0x1000;
        const LOAD_GPA: u64 = 0x10_0000;

        let mut image = vec![0; DATA_OFFSET + 4];
        image[..6].copy_from_slice(b"\x7fELF\x02\x01");
        image[6] = 1;
        write_u16(&mut image, 16, 2);
        write_u16(&mut image, 18, elf::EM_X86_64);
        write_u32(&mut image, 20, 1);
        write_u64(&mut image, 24, LOAD_GPA);
        write_u64(&mut image, 32, 64);
        write_u16(&mut image, 52, 64);
        write_u16(&mut image, 54, 56);
        write_u16(&mut image, 56, 2);

        write_u32(&mut image, 64, elf::PT_LOAD);
        write_u64(&mut image, 64 + 8, DATA_OFFSET as u64);
        write_u64(&mut image, 64 + 16, LOAD_GPA);
        write_u64(&mut image, 64 + 24, LOAD_GPA);
        write_u64(&mut image, 64 + 32, 4);
        write_u64(&mut image, 64 + 40, HV_PAGE_SIZE);
        write_u64(&mut image, 64 + 48, HV_PAGE_SIZE);

        let note = &mut image[NOTE_OFFSET..NOTE_OFFSET + 20];
        note[0..4].copy_from_slice(&4u32.to_le_bytes());
        note[4..8].copy_from_slice(&4u32.to_le_bytes());
        note[8..12].copy_from_slice(&XEN_ELFNOTE_PHYS32_ENTRY.to_le_bytes());
        note[12..16].copy_from_slice(b"Xen\0");
        note[16..20].copy_from_slice(&(LOAD_GPA as u32).to_le_bytes());

        write_u32(&mut image, 120, elf::PT_NOTE);
        write_u64(&mut image, 120 + 8, NOTE_OFFSET as u64);
        write_u64(&mut image, 120 + 32, 20);
        write_u64(&mut image, 120 + 40, 20);
        image[DATA_OFFSET..].copy_from_slice(&[1, 2, 3, 4]);
        image
    }

    #[test]
    fn pvh_structures_match_xen_abi() {
        assert_eq!(size_of::<HvmStartInfo>(), 56);
        assert_eq!(size_of::<HvmModlistEntry>(), 32);
        assert_eq!(size_of::<HvmMemmapTableEntry>(), 24);
    }

    #[test]
    fn reserves_configured_memory_ranges() {
        let shared_status = MemoryRange::new(0x3_0000..0x3_1000);
        let entries = pvh_memory_map(&make_layout(), &[shared_status]).unwrap();
        let entries = entries
            .iter()
            .map(|entry| (entry.addr, entry.size, entry.entry_type))
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            [
                (0, 0x3_0000, XEN_HVM_MEMMAP_TYPE_RAM),
                (0x3_0000, 0x1000, XEN_HVM_MEMMAP_TYPE_RESERVED),
                (
                    0x3_1000,
                    64 * 1024 * 1024 - 0x3_1000,
                    XEN_HVM_MEMMAP_TYPE_RAM
                ),
            ]
        );

        assert!(matches!(
            pvh_memory_map(
                &make_layout(),
                &[MemoryRange::new(
                    64 * 1024 * 1024..64 * 1024 * 1024 + 0x1000
                )]
            ),
            Err(Error::InvalidReservedMemoryRange { .. })
        ));
    }

    #[test]
    fn loads_segments_zeroes_bss_and_sets_entry_state() {
        let mut kernel = Cursor::new(test_elf());
        let mut initrd = Cursor::new(vec![0x5a; 17]);
        let mut importer = RecordingImporter::default();
        let info = load(
            &mut importer,
            &mut kernel,
            Some(InitrdConfig {
                image: &mut initrd,
                size: 17,
            }),
            "earlycon=xe9",
            &make_layout(),
            None,
        )
        .unwrap();

        assert_eq!(info.entrypoint, 0x10_0000);
        assert!(info.initrd.is_some());
        let kernel_import = importer
            .pages
            .iter()
            .find(|(tag, ..)| *tag == "pvh-kernel")
            .unwrap();
        assert_eq!(&kernel_import.3[..4], &[1, 2, 3, 4]);
        assert!(kernel_import.3[4..].iter().all(|byte| *byte == 0));
        assert!(
            importer
                .registers
                .contains(&X86Register::Rbx(START_INFO_ADDR))
        );
        assert!(importer.registers.contains(&X86Register::Rip(0x10_0000)));
        assert!(importer.registers.contains(&X86Register::Cr0(1)));
    }

    #[test]
    fn exposes_platform_tables() {
        let mut kernel = Cursor::new(test_elf());
        let mut importer = RecordingImporter::default();
        let acpi_tables = AcpiTables {
            rsdp: vec![0x5a; 36],
            tables: vec![0xa5; 17],
        };
        load::<_, Cursor<Vec<u8>>>(
            &mut importer,
            &mut kernel,
            None,
            "",
            &make_layout(),
            Some(&acpi_tables),
        )
        .unwrap();

        let start_info = &importer
            .pages
            .iter()
            .find(|(tag, ..)| *tag == "pvh-start-info")
            .unwrap()
            .3;
        assert_eq!(
            u64::from_le_bytes(start_info[32..40].try_into().unwrap()),
            ACPI_RSDP_ADDR
        );

        let rsdp = importer
            .pages
            .iter()
            .find(|(tag, ..)| *tag == "pvh-acpi-rsdp")
            .unwrap();
        assert_eq!(rsdp.1, ACPI_RSDP_ADDR / HV_PAGE_SIZE);
        assert_eq!(&rsdp.3[..36], &[0x5a; 36]);

        let tables = importer
            .pages
            .iter()
            .find(|(tag, ..)| *tag == "pvh-acpi-tables")
            .unwrap();
        assert_eq!(tables.1, ACPI_TABLES_ADDR / HV_PAGE_SIZE);
        assert_eq!(&tables.3[..17], &[0xa5; 17]);

        let boot_tables = &importer
            .pages
            .iter()
            .find(|(tag, ..)| *tag == "pvh-boot-tables")
            .unwrap()
            .3;
        let floating_pointer = &boot_tables[..16];
        assert_eq!(&floating_pointer[..4], b"_MP_");
        assert_eq!(
            u32::from_le_bytes(floating_pointer[4..8].try_into().unwrap()),
            MP_CONFIG_TABLE_ADDR as u32
        );
        assert_eq!(
            floating_pointer
                .iter()
                .copied()
                .fold(0u8, |sum, byte| sum.wrapping_add(byte)),
            0
        );

        let table_length = u16::from_le_bytes(
            boot_tables[MP_CONFIG_TABLE_ADDR + 4..MP_CONFIG_TABLE_ADDR + 6]
                .try_into()
                .unwrap(),
        ) as usize;
        let mp_table = &boot_tables[MP_CONFIG_TABLE_ADDR..MP_CONFIG_TABLE_ADDR + table_length];
        assert_eq!(&mp_table[..4], b"PCMP");
        assert_eq!(
            mp_table
                .iter()
                .copied()
                .fold(0u8, |sum, byte| sum.wrapping_add(byte)),
            0
        );
        for irq in (0..16).filter(|irq| *irq != 2) {
            let entry = mp_table[80..]
                .chunks_exact(8)
                .find(|entry| entry[0] == 3 && entry[5] == irq)
                .unwrap();
            assert_eq!(u16::from_le_bytes(entry[2..4].try_into().unwrap()), 0);
        }
    }

    #[test]
    fn emits_smp_mp_processor_entries() {
        const LEVEL_TRIGGERED_IRQS: &[u32] = &[4, 5, 6, 7, 9, 10, 11, 12];

        for processor_count in [1usize, 2, 4, 8] {
            let apic_ids = (0..processor_count as u32).collect::<Vec<_>>();
            let boot_config = BootConfig {
                apic_ids: &apic_ids,
                level_triggered_irqs: LEVEL_TRIGGERED_IRQS,
                reserved_memory_ranges: &[],
            };
            let mut boot_page = [0; HV_PAGE_SIZE as usize];
            write_mp_tables(&mut boot_page, &boot_config).unwrap();

            let table_length = u16::from_le_bytes(
                boot_page[MP_CONFIG_TABLE_ADDR + 4..MP_CONFIG_TABLE_ADDR + 6]
                    .try_into()
                    .unwrap(),
            ) as usize;
            assert_eq!(table_length, 180 + 20 * processor_count);
            assert!(MP_CONFIG_TABLE_ADDR + table_length <= BOOT_GDT_ADDR as usize);

            let table = &boot_page[MP_CONFIG_TABLE_ADDR..MP_CONFIG_TABLE_ADDR + table_length];
            assert_eq!(
                u16::from_le_bytes(table[34..36].try_into().unwrap()),
                u16::try_from(17 + processor_count).unwrap()
            );
            assert_eq!(
                table
                    .iter()
                    .copied()
                    .fold(0u8, |sum, byte| sum.wrapping_add(byte)),
                0
            );

            for (index, entry) in table[44..44 + 20 * processor_count]
                .chunks_exact(20)
                .enumerate()
            {
                assert_eq!(entry[0], 0);
                assert_eq!(entry[1], index as u8);
                assert_eq!(entry[2], 0x14);
                assert_eq!(entry[3], if index == 0 { 3 } else { 1 });
            }
        }
    }

    #[test]
    fn rejects_invalid_mp_topology_and_gdt_overlap() {
        let mut boot_page = [0; HV_PAGE_SIZE as usize];
        assert!(matches!(
            write_mp_tables(
                &mut boot_page,
                &BootConfig {
                    apic_ids: &[],
                    level_triggered_irqs: &[],
                    reserved_memory_ranges: &[],
                }
            ),
            Err(Error::NoProcessors)
        ));
        assert!(matches!(
            write_mp_tables(
                &mut boot_page,
                &BootConfig {
                    apic_ids: &[0, 2],
                    level_triggered_irqs: &[],
                    reserved_memory_ranges: &[],
                }
            ),
            Err(Error::InvalidApicId { .. })
        ));
        assert!(matches!(
            write_mp_tables(
                &mut boot_page,
                &BootConfig {
                    apic_ids: &(0..100).collect::<Vec<_>>(),
                    level_triggered_irqs: &[],
                    reserved_memory_ranges: &[],
                }
            ),
            Err(Error::MpTableOverlap { .. })
        ));
    }

    #[test]
    fn rejects_malformed_note_and_command_line() {
        assert!(matches!(
            find_pvh_entry(&[0; 11]),
            Err(Error::MalformedNote)
        ));

        let mut duplicate = vec![0; 40];
        for offset in [0, 20] {
            duplicate[offset..offset + 4].copy_from_slice(&4u32.to_le_bytes());
            duplicate[offset + 4..offset + 8].copy_from_slice(&4u32.to_le_bytes());
            duplicate[offset + 8..offset + 12]
                .copy_from_slice(&XEN_ELFNOTE_PHYS32_ENTRY.to_le_bytes());
            duplicate[offset + 12..offset + 16].copy_from_slice(b"Xen\0");
            duplicate[offset + 16..offset + 20].copy_from_slice(&0x10_0000u32.to_le_bytes());
        }
        assert!(matches!(
            find_pvh_entry(&duplicate),
            Err(Error::DuplicatePvhEntry)
        ));

        let mut kernel = Cursor::new(test_elf());
        let mut importer = RecordingImporter::default();
        let error = load::<_, Cursor<Vec<u8>>>(
            &mut importer,
            &mut kernel,
            None,
            &"x".repeat(CMDLINE_MAX_SIZE),
            &make_layout(),
            None,
        )
        .unwrap_err();
        assert!(matches!(error, Error::CommandLineTooLong));

        let mut kernel = Cursor::new(test_elf());
        let mut importer = RecordingImporter::default();
        let error = load::<_, Cursor<Vec<u8>>>(
            &mut importer,
            &mut kernel,
            None,
            "bad\0command-line",
            &make_layout(),
            None,
        )
        .unwrap_err();
        assert!(matches!(error, Error::CommandLineNul));
    }

    #[test]
    fn rejects_bad_segments_and_missing_entry() {
        let mut missing_note = test_elf();
        write_u32(&mut missing_note, 120, elf::PT_NULL);
        assert!(matches!(
            parse_kernel(&mut Cursor::new(missing_note)),
            Err(Error::MissingPvhEntry)
        ));

        let mut overlap = test_elf();
        write_u32(&mut overlap, 120, elf::PT_LOAD);
        write_u64(&mut overlap, 120 + 8, 0x1000);
        write_u64(&mut overlap, 120 + 24, 0x10_0000);
        write_u64(&mut overlap, 120 + 32, 4);
        write_u64(&mut overlap, 120 + 40, HV_PAGE_SIZE);
        assert!(matches!(
            parse_kernel(&mut Cursor::new(overlap)),
            Err(Error::OverlappingLoadSegments)
        ));

        let mut overflow = test_elf();
        write_u64(&mut overflow, 64 + 24, u64::MAX - 1);
        assert!(matches!(
            parse_kernel(&mut Cursor::new(overflow)),
            Err(Error::ProgramHeaderOverflow)
        ));

        let mut oversized_file = test_elf();
        write_u64(&mut oversized_file, 64 + 40, 3);
        assert!(matches!(
            parse_kernel(&mut Cursor::new(oversized_file)),
            Err(Error::FileSizeExceedsMemorySize)
        ));
    }

    #[test]
    fn rejects_initrd_collision() {
        let mut kernel = Cursor::new(test_elf());
        let mut initrd = Cursor::new(Vec::new());
        let mut importer = RecordingImporter::default();
        let error = load(
            &mut importer,
            &mut kernel,
            Some(InitrdConfig {
                image: &mut initrd,
                size: make_layout().ram_size(),
            }),
            "",
            &make_layout(),
            None,
        )
        .unwrap_err();
        assert!(matches!(error, Error::InitrdDoesNotFit));
    }

    #[test]
    fn high_kernel_segment_does_not_block_low_initrd() {
        let layout = MemoryLayout::new(
            8 * 1024 * 1024 * 1024,
            &[MemoryRange::new(0xc000_0000..FOUR_GB)],
            &[],
            &[],
            None,
        )
        .unwrap();
        let segments = [
            Segment {
                file_offset: 0,
                file_size: HV_PAGE_SIZE,
                gpa: HIMEM_START,
                memory_size: HV_PAGE_SIZE,
            },
            Segment {
                file_offset: HV_PAGE_SIZE,
                file_size: HV_PAGE_SIZE,
                gpa: FOUR_GB,
                memory_size: HV_PAGE_SIZE,
            },
        ];

        let base = place_initrd(&layout, HV_PAGE_SIZE, &segments).unwrap();
        assert_eq!(base, 0xc000_0000 - HV_PAGE_SIZE);
    }
}
