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
use memory_range::subtract_ranges;
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
    /// 8-bit APIC IDs in virtual-processor order. The first entry is the BSP.
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
    #[error("ELF load segment overlaps PVH reserved memory")]
    SegmentOverlapsReservedMemory,
    #[error("{tag} overlaps PVH reserved memory")]
    BootStructureOverlapsReservedMemory { tag: &'static str },
    #[error("required guest range {start:#x}..{end:#x} for {tag} is outside declared RAM")]
    OutsideRam {
        tag: &'static str,
        start: u64,
        end: u64,
    },
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
    #[error("PVH processor {index} has APIC ID {apic_id}, which does not fit in 8 bits")]
    InvalidApicId { index: usize, apic_id: u32 },
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
    #[error("PVH register {0:?} is not supported by this loader backend")]
    UnsupportedRegister(X86Register),
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

struct BootPages {
    page_base: u64,
    tag: &'static str,
    data: Vec<u8>,
}

impl BootPages {
    fn page_count(&self) -> u64 {
        (self.data.len() as u64).div_ceil(HV_PAGE_SIZE)
    }
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
    for register in initial_registers(0) {
        if !importer.supports_vp_register(&register) {
            return Err(Error::UnsupportedRegister(register));
        }
    }
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

    let memory_ranges = pvh_memory_map(memory_layout, boot_config.reserved_memory_ranges)?;
    for segment in &segments {
        let (page_base, page_count) = segment.page_span()?;
        if overlaps_reserved_memory(page_base, page_count, boot_config.reserved_memory_ranges)? {
            return Err(Error::SegmentOverlapsReservedMemory);
        }
        verify_ram(memory_layout, page_base, page_count, "pvh-kernel")?;
    }
    let initrd = match initrd {
        Some(initrd) => {
            if initrd.size == 0 {
                return Err(Error::EmptyInitrd);
            }
            let base = place_initrd(
                memory_layout,
                initrd.size,
                &segments,
                boot_config.reserved_memory_ranges,
            )?;
            let (page_base, page_count) = page_span(base, initrd.size)?;
            verify_ram(memory_layout, page_base, page_count, "pvh-initrd")?;
            Some((initrd, base))
        }
        None => None,
    };

    let boot_pages = build_boot_structures(
        cmdline,
        &memory_ranges,
        initrd.as_ref().map(|(initrd, base)| (*base, initrd.size)),
        acpi_tables,
        boot_config,
    )?;
    for pages in &boot_pages {
        if overlaps_reserved_memory(
            pages.page_base,
            pages.page_count(),
            boot_config.reserved_memory_ranges,
        )? {
            return Err(Error::BootStructureOverlapsReservedMemory { tag: pages.tag });
        }
        verify_ram(
            memory_layout,
            pages.page_base,
            pages.page_count(),
            pages.tag,
        )?;
    }

    for segment in &segments {
        let (page_base, page_count) = segment.page_span()?;
        verify_memory(importer, page_base, page_count, "pvh-kernel")?;
    }
    if let Some((initrd, base)) = &initrd {
        let (page_base, page_count) = page_span(*base, initrd.size)?;
        verify_memory(importer, page_base, page_count, "pvh-initrd")?;
    }
    for pages in &boot_pages {
        verify_memory(importer, pages.page_base, pages.page_count(), pages.tag)?;
    }

    let mut chunk = ChunkBuf::new();
    for segment in &segments {
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
        Some((initrd, base)) => {
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

    for pages in boot_pages {
        importer
            .import_pages(
                pages.page_base,
                pages.page_count(),
                pages.tag,
                BootPageAcceptance::Exclusive,
                &pages.data,
            )
            .map_err(|source| Error::ImportPages {
                tag: pages.tag,
                source,
            })?;
    }
    import_registers(importer, entrypoint)?;

    Ok(LoadInfo { entrypoint, initrd })
}

fn place_initrd(
    memory_layout: &MemoryLayout,
    size: u64,
    segments: &[Segment],
    reserved_ranges: &[MemoryRange],
) -> Result<u64, Error> {
    'candidate: for low_range in memory_layout
        .ram()
        .iter()
        .rev()
        .filter(|range| range.range.start() < FOUR_GB)
    {
        let low_end = low_range.range.end().min(FOUR_GB);
        let Some(unaligned_base) = low_end.checked_sub(size) else {
            continue;
        };
        let base = unaligned_base & !(HV_PAGE_SIZE - 1);
        if base < HIMEM_START || base < low_range.range.start() {
            continue;
        }

        let (initrd_page_base, initrd_page_count) = page_span(base, size)?;
        if overlaps_reserved_memory(initrd_page_base, initrd_page_count, reserved_ranges)? {
            continue;
        }
        let initrd_page_end = initrd_page_base
            .checked_add(initrd_page_count)
            .ok_or(Error::AddressOverflow)?;
        for segment in segments {
            let (segment_page_base, segment_page_count) = segment.page_span()?;
            let segment_page_end = segment_page_base
                .checked_add(segment_page_count)
                .ok_or(Error::AddressOverflow)?;
            if initrd_page_base < segment_page_end && segment_page_base < initrd_page_end {
                continue 'candidate;
            }
        }
        return Ok(base);
    }
    Err(Error::InitrdDoesNotFit)
}

fn overlaps_reserved_memory(
    page_base: u64,
    page_count: u64,
    reserved_ranges: &[MemoryRange],
) -> Result<bool, Error> {
    let page_end = page_base
        .checked_add(page_count)
        .ok_or(Error::AddressOverflow)?;
    Ok(reserved_ranges.iter().any(|range| {
        page_base < range.end() / HV_PAGE_SIZE && range.start() / HV_PAGE_SIZE < page_end
    }))
}

fn verify_ram(
    memory_layout: &MemoryLayout,
    page_base: u64,
    page_count: u64,
    tag: &'static str,
) -> Result<(), Error> {
    let start = page_base
        .checked_mul(HV_PAGE_SIZE)
        .ok_or(Error::AddressOverflow)?;
    let end = page_base
        .checked_add(page_count)
        .and_then(|end| end.checked_mul(HV_PAGE_SIZE))
        .ok_or(Error::AddressOverflow)?;
    if subtract_ranges(
        [MemoryRange::new(start..end)],
        memory_layout.ram().iter().map(|ram| ram.range),
    )
    .next()
    .is_some()
    {
        return Err(Error::OutsideRam { tag, start, end });
    }
    Ok(())
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

        if note_type == XEN_ELFNOTE_PHYS32_ENTRY && name == b"Xen\0" {
            let entry = match descriptor.len() {
                4 => u64::from(u32::from_le_bytes(
                    descriptor.try_into().map_err(|_| Error::MalformedNote)?,
                )),
                8 => u64::from_le_bytes(descriptor.try_into().map_err(|_| Error::MalformedNote)?),
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

fn build_boot_structures(
    cmdline: &str,
    memory_ranges: &[HvmMemmapTableEntry],
    initrd: Option<(u64, u64)>,
    acpi_tables: Option<&AcpiTables>,
    boot_config: &BootConfig<'_>,
) -> Result<Vec<BootPages>, Error> {
    let mut pages = Vec::new();
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
    pages.push(BootPages {
        page_base: 0,
        tag: "pvh-boot-tables",
        data: boot_page.to_vec(),
    });

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
    pages.push(BootPages {
        page_base: MEMMAP_ADDR / HV_PAGE_SIZE,
        tag: "pvh-memory-map",
        data: memmap_page.to_vec(),
    });

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
    pages.push(BootPages {
        page_base: START_INFO_ADDR / HV_PAGE_SIZE,
        tag: "pvh-start-info",
        data: start_page.to_vec(),
    });

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
        pages.push(BootPages {
            page_base: ACPI_RSDP_ADDR / HV_PAGE_SIZE,
            tag: "pvh-acpi-rsdp",
            data: rsdp_page.to_vec(),
        });

        let table_pages = (acpi_tables.tables.len() as u64).div_ceil(HV_PAGE_SIZE);
        let mut table_data = vec![0; (table_pages * HV_PAGE_SIZE) as usize];
        table_data[..acpi_tables.tables.len()].copy_from_slice(&acpi_tables.tables);
        if table_pages != 0 {
            pages.push(BootPages {
                page_base: ACPI_TABLES_ADDR / HV_PAGE_SIZE,
                tag: "pvh-acpi-tables",
                data: table_data,
            });
        }
    }

    let cmdline_size = cmdline.len().checked_add(1).ok_or(Error::AddressOverflow)?;
    let cmdline_pages = (cmdline_size as u64).div_ceil(HV_PAGE_SIZE);
    let mut cmdline_data = vec![0; (cmdline_pages * HV_PAGE_SIZE) as usize];
    cmdline_data[..cmdline.len()].copy_from_slice(cmdline.as_bytes());
    pages.push(BootPages {
        page_base: CMDLINE_ADDR / HV_PAGE_SIZE,
        tag: "pvh-command-line",
        data: cmdline_data,
    });

    Ok(pages)
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
            && subtract_ranges([*range], memory_layout.ram().iter().map(|ram| ram.range))
                .next()
                .is_none();
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
            let start = reserved.start().max(ram.range.start());
            let end = reserved.end().min(ram.range.end());
            if start >= end {
                continue;
            }
            if next < start {
                entries.push(HvmMemmapTableEntry {
                    addr: next,
                    size: start - next,
                    entry_type: XEN_HVM_MEMMAP_TYPE_RAM,
                    reserved: 0,
                });
            }
            entries.push(HvmMemmapTableEntry {
                addr: start,
                size: end - start,
                entry_type: XEN_HVM_MEMMAP_TYPE_RESERVED,
                reserved: 0,
            });
            next = end;
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
        let apic_id = u8::try_from(apic_id).map_err(|_| Error::InvalidApicId { index, apic_id })?;
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

    let table_len = u16::try_from(table.len()).map_err(|_| Error::TooManyMpEntries)?;
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

fn initial_registers(entrypoint: u64) -> [X86Register; 17] {
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
    [
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
    ]
}

fn import_registers(
    importer: &mut dyn ImageLoad<X86Register>,
    entrypoint: u64,
) -> Result<(), Error> {
    for register in initial_registers(entrypoint) {
        importer
            .import_vp_register(register)
            .map_err(|source| Error::ImportPages {
                tag: "pvh-registers",
                source,
            })?;
    }
    Ok(())
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
    use test_with_tracing::test;

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
        unsupported_register: Option<X86Register>,
        memory_checks: usize,
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

        fn supports_vp_register(&self, register: &X86Register) -> bool {
            self.unsupported_register.is_none_or(|unsupported| {
                std::mem::discriminant(&unsupported) != std::mem::discriminant(register)
            })
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
            self.memory_checks += 1;
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
    fn rejects_each_unsupported_register_before_import() {
        for register in initial_registers(0) {
            let mut importer = RecordingImporter {
                unsupported_register: Some(register),
                ..Default::default()
            };
            let error = load::<_, Cursor<Vec<u8>>>(
                &mut importer,
                &mut Cursor::new(Vec::<u8>::new()),
                None,
                "",
                &make_layout(),
                None,
            )
            .unwrap_err();
            assert!(matches!(
                error,
                Error::UnsupportedRegister(unsupported) if unsupported == register
            ));
            assert!(importer.pages.is_empty());
            assert!(importer.registers.is_empty());
        }
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
    fn reservations_span_contiguous_numa_extents_but_not_ram_holes() {
        let boundary = 2 * 1024 * 1024;
        let layout =
            MemoryLayout::new_with_numa(&[boundary, HV_PAGE_SIZE, boundary], &[], &[], &[], None)
                .unwrap();
        let reserved = MemoryRange::new(boundary - HV_PAGE_SIZE..boundary + 2 * HV_PAGE_SIZE);
        let entries = pvh_memory_map(&layout, &[reserved]).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.addr, entry.size, entry.entry_type))
                .collect::<Vec<_>>(),
            [
                (0, boundary - HV_PAGE_SIZE, XEN_HVM_MEMMAP_TYPE_RAM),
                (
                    boundary - HV_PAGE_SIZE,
                    HV_PAGE_SIZE,
                    XEN_HVM_MEMMAP_TYPE_RESERVED
                ),
                (boundary, HV_PAGE_SIZE, XEN_HVM_MEMMAP_TYPE_RESERVED),
                (
                    boundary + HV_PAGE_SIZE,
                    HV_PAGE_SIZE,
                    XEN_HVM_MEMMAP_TYPE_RESERVED
                ),
                (
                    boundary + 2 * HV_PAGE_SIZE,
                    boundary - HV_PAGE_SIZE,
                    XEN_HVM_MEMMAP_TYPE_RAM
                ),
            ]
        );
        let split_reservations = [
            MemoryRange::new(reserved.start()..boundary),
            MemoryRange::new(boundary..boundary + HV_PAGE_SIZE),
            MemoryRange::new(boundary + HV_PAGE_SIZE..reserved.end()),
        ];
        assert_eq!(
            entries.as_bytes(),
            pvh_memory_map(&layout, &split_reservations)
                .unwrap()
                .as_bytes()
        );

        let layout_with_hole = MemoryLayout::new_with_numa(
            &[boundary, HV_PAGE_SIZE, boundary],
            &[MemoryRange::new(boundary..boundary + HV_PAGE_SIZE)],
            &[],
            &[],
            None,
        )
        .unwrap();
        assert!(matches!(
            pvh_memory_map(&layout_with_hole, &[reserved]),
            Err(Error::InvalidReservedMemoryRange { start, end })
                if start == reserved.start() && end == reserved.end()
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

        for apic_ids in [
            vec![0],
            vec![13, 14],
            vec![0, 2, 127, 255],
            (0..8).collect(),
        ] {
            let processor_count = apic_ids.len();
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
                assert_eq!(u32::from(entry[1]), apic_ids[index]);
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
                    apic_ids: &[0, 256],
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
    fn rejects_oversized_mp_table() {
        assert!(matches!(
            build_mp_config_table(&BootConfig {
                apic_ids: &vec![0; usize::from(u16::MAX) / 20],
                level_triggered_irqs: &[],
                reserved_memory_ranges: &[],
            }),
            Err(Error::TooManyMpEntries)
        ));
    }

    #[test]
    fn pvh_entry_note_requires_exact_owner_and_numeric_descriptor() {
        for name in [
            b"Xen\0".as_slice(),
            b"Xen",
            b"XenFoo\0",
            b"Xen\0\0",
            b"GNU\0",
        ] {
            for descriptor_size in [0usize, 3, 4, 5, 8] {
                let descriptor_start = align_up_usize(12 + name.len(), 4).unwrap();
                let mut note =
                    vec![0; align_up_usize(descriptor_start + descriptor_size, 4).unwrap()];
                write_u32(&mut note, 0, name.len() as u32);
                write_u32(&mut note, 4, descriptor_size as u32);
                write_u32(&mut note, 8, XEN_ELFNOTE_PHYS32_ENTRY);
                note[12..12 + name.len()].copy_from_slice(name);
                note[descriptor_start..descriptor_start + descriptor_size]
                    .copy_from_slice(&HIMEM_START.to_le_bytes()[..descriptor_size]);

                let result = find_pvh_entry(&note);
                if name != b"Xen\0" {
                    assert!(result.unwrap().is_none());
                } else if matches!(descriptor_size, 4 | 8) {
                    assert_eq!(result.unwrap(), Some(HIMEM_START));
                } else {
                    assert!(matches!(result, Err(Error::MalformedNote)));
                }
            }
        }
    }

    #[test]
    fn eight_byte_entry_note_still_requires_a_32_bit_address() {
        for entrypoint in [HIMEM_START, FOUR_GB] {
            let mut image = test_elf();
            write_u32(&mut image, 0x200 + 4, 8);
            write_u64(&mut image, 0x200 + 16, entrypoint);
            write_u64(&mut image, 120 + 32, 24);
            write_u64(&mut image, 120 + 40, 24);
            let result = parse_kernel(&mut Cursor::new(image));
            if entrypoint < FOUR_GB {
                assert_eq!(result.unwrap().entrypoint, entrypoint);
            } else {
                assert!(matches!(result, Err(Error::EntryAboveFourGb)));
            }
        }
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
    fn rejects_reserved_kernel_pages_before_import() {
        let mut image = test_elf();
        write_u16(&mut image, 56, 3);
        write_u32(&mut image, 176, elf::PT_LOAD);
        write_u64(&mut image, 176 + 8, 0x1000);
        write_u64(&mut image, 176 + 24, HIMEM_START + 2 * HV_PAGE_SIZE);
        write_u64(&mut image, 176 + 32, 4);
        write_u64(&mut image, 176 + 40, HV_PAGE_SIZE + 1);

        let mut importer = RecordingImporter::default();
        let error = load_with_boot_config::<_, Cursor<Vec<u8>>>(
            &mut importer,
            &mut Cursor::new(image),
            None,
            "",
            &make_layout(),
            None,
            &BootConfig {
                apic_ids: &[0],
                level_triggered_irqs: &[],
                reserved_memory_ranges: &[MemoryRange::new(
                    HIMEM_START + 3 * HV_PAGE_SIZE..HIMEM_START + 4 * HV_PAGE_SIZE,
                )],
            },
        )
        .unwrap_err();
        assert!(matches!(error, Error::SegmentOverlapsReservedMemory));
        assert!(importer.pages.is_empty());
        assert!(importer.registers.is_empty());
    }

    #[test]
    fn initrd_respects_reserved_page_boundaries() {
        let layout = make_layout();
        let ram_end = layout.ram_size();
        for reserved_start in [ram_end - HV_PAGE_SIZE, ram_end - 2 * HV_PAGE_SIZE] {
            let mut importer = RecordingImporter::default();
            let result = load_with_boot_config(
                &mut importer,
                &mut Cursor::new(test_elf()),
                Some(InitrdConfig {
                    image: &mut Cursor::new(vec![0x5a; 17]),
                    size: 17,
                }),
                "",
                &layout,
                None,
                &BootConfig {
                    apic_ids: &[0],
                    level_triggered_irqs: &[],
                    reserved_memory_ranges: &[MemoryRange::new(
                        reserved_start..reserved_start + HV_PAGE_SIZE,
                    )],
                },
            );
            if reserved_start == ram_end - HV_PAGE_SIZE {
                assert!(matches!(result, Err(Error::InitrdDoesNotFit)));
                assert!(importer.pages.is_empty());
                assert!(importer.registers.is_empty());
            } else {
                assert_eq!(result.unwrap().initrd, Some((ram_end - HV_PAGE_SIZE, 17)));
            }
        }
    }

    #[test]
    fn rejects_metadata_reservations_and_ram_holes_before_import() {
        let cmdline = "x".repeat(HV_PAGE_SIZE as usize);
        let acpi_tables = AcpiTables {
            rsdp: vec![0; 36],
            tables: vec![0; HV_PAGE_SIZE as usize + 1],
        };
        for (base, tag) in [
            (0, "pvh-boot-tables"),
            (START_INFO_ADDR, "pvh-start-info"),
            (MEMMAP_ADDR, "pvh-memory-map"),
            (ACPI_RSDP_ADDR, "pvh-acpi-rsdp"),
            (ACPI_TABLES_ADDR, "pvh-acpi-tables"),
            (ACPI_TABLES_ADDR + HV_PAGE_SIZE, "pvh-acpi-tables"),
            (CMDLINE_ADDR, "pvh-command-line"),
            (CMDLINE_ADDR + HV_PAGE_SIZE, "pvh-command-line"),
        ] {
            let range = MemoryRange::new(base..base + HV_PAGE_SIZE);
            let reserved_ranges = [range];
            for is_reserved in [true, false] {
                let layout = if is_reserved {
                    make_layout()
                } else {
                    MemoryLayout::new(64 * 1024 * 1024, &[range], &[], &[], None).unwrap()
                };
                let mut importer = RecordingImporter::default();
                let error = load_with_boot_config::<_, Cursor<Vec<u8>>>(
                    &mut importer,
                    &mut Cursor::new(test_elf()),
                    None,
                    &cmdline,
                    &layout,
                    Some(&acpi_tables),
                    &BootConfig {
                        apic_ids: &[0],
                        level_triggered_irqs: &[],
                        reserved_memory_ranges: if is_reserved { &reserved_ranges } else { &[] },
                    },
                )
                .unwrap_err();
                if is_reserved {
                    assert!(matches!(
                        error,
                        Error::BootStructureOverlapsReservedMemory { tag: actual } if actual == tag
                    ));
                } else {
                    assert!(matches!(
                        error,
                        Error::OutsideRam { tag: actual, .. } if actual == tag
                    ));
                }
                assert!(importer.pages.is_empty());
                assert!(importer.registers.is_empty());
                assert_eq!(importer.memory_checks, 0);
            }
        }
    }

    #[test]
    fn checks_kernel_page_spans_against_declared_ram() {
        let kernel_start = HIMEM_START - HV_PAGE_SIZE;
        let kernel_end = HIMEM_START + 2 * HV_PAGE_SIZE;
        let hole = MemoryRange::new(HIMEM_START..HIMEM_START + HV_PAGE_SIZE);
        let layouts = [
            MemoryLayout::new(64 * 1024 * 1024, &[hole], &[], &[], None).unwrap(),
            MemoryLayout::new(HIMEM_START + HV_PAGE_SIZE, &[], &[], &[], None).unwrap(),
            MemoryLayout::new_with_numa(
                &[HIMEM_START + HV_PAGE_SIZE, 64 * 1024 * 1024],
                &[],
                &[],
                &[],
                None,
            )
            .unwrap(),
        ];
        for (index, layout) in layouts.iter().enumerate() {
            let mut image = test_elf();
            write_u16(&mut image, 56, 3);
            write_u32(&mut image, 176, elf::PT_LOAD);
            write_u64(&mut image, 176 + 8, 0x1000);
            write_u64(&mut image, 176 + 24, HIMEM_START + HV_PAGE_SIZE);
            write_u64(&mut image, 176 + 32, 1);
            write_u64(&mut image, 176 + 40, 1);
            let mut importer = RecordingImporter::default();
            let result = load::<_, Cursor<Vec<u8>>>(
                &mut importer,
                &mut Cursor::new(image),
                None,
                "",
                layout,
                None,
            );
            if index < 2 {
                assert!(
                    matches!(
                        &result,
                        Err(Error::OutsideRam {
                            tag: "pvh-kernel",
                            ..
                        })
                    ),
                    "layout {index}: {result:?}"
                );
                assert!(importer.pages.is_empty());
                assert!(importer.registers.is_empty());
                assert_eq!(importer.memory_checks, 0);
            } else {
                result.unwrap();
            }

            let span = page_span(kernel_start, kernel_end - kernel_start).unwrap();
            let result = verify_ram(layout, span.0, span.1, "pvh-kernel");
            assert_eq!(result.is_ok(), index == 2);
        }
    }

    #[test]
    fn initrd_placement_tries_lower_ram_extents() {
        let low_end = 4 * 1024 * 1024;
        let layouts = [
            MemoryLayout::new_with_numa(&[low_end, HV_PAGE_SIZE], &[], &[], &[], None).unwrap(),
            MemoryLayout::new(
                low_end + HV_PAGE_SIZE,
                &[MemoryRange::new(low_end..2 * low_end)],
                &[],
                &[],
                None,
            )
            .unwrap(),
        ];
        for layout in &layouts {
            let upper_end = layout.ram().last().unwrap().range.end();
            let upper_base = upper_end - HV_PAGE_SIZE;
            let lower_base = low_end - HV_PAGE_SIZE;
            assert_eq!(
                place_initrd(layout, HV_PAGE_SIZE + 1, &[], &[]).unwrap(),
                low_end - 2 * HV_PAGE_SIZE
            );
            assert_eq!(place_initrd(layout, 17, &[], &[]).unwrap(), upper_base);

            let upper_segment = Segment {
                file_offset: 0,
                file_size: 1,
                gpa: upper_base,
                memory_size: 1,
            };
            assert_eq!(
                place_initrd(layout, 17, &[upper_segment], &[]).unwrap(),
                lower_base
            );
            let reserved = [
                MemoryRange::new(lower_base..low_end),
                MemoryRange::new(upper_base..upper_end),
            ];
            assert_eq!(
                place_initrd(layout, 17, &[], &reserved[1..]).unwrap(),
                lower_base
            );
            assert!(matches!(
                place_initrd(layout, 17, &[], &reserved),
                Err(Error::InitrdDoesNotFit)
            ));
            assert!(matches!(
                place_initrd(
                    layout,
                    17,
                    &[
                        Segment {
                            gpa: lower_base,
                            ..upper_segment
                        },
                        upper_segment,
                    ],
                    &[],
                ),
                Err(Error::InitrdDoesNotFit)
            ));
        }
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

        let base = place_initrd(&layout, HV_PAGE_SIZE, &segments, &[]).unwrap();
        assert_eq!(base, 0xc000_0000 - HV_PAGE_SIZE);
    }
}
