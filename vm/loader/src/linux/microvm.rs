// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux direct boot for the microVM MP-table machine profile.

use super::CR3_BASE;
use super::Error;
use super::GDT_BASE;
use super::InitrdAddressType;
use super::InitrdConfig;
use super::InitrdInfo;
use super::KERNEL_BASE;
use super::KernelInfo;
use super::LoadInfo;
use super::ZERO_PAGE_BASE;
use crate::common::ChunkBuf;
use crate::common::ImportFileRegion;
use crate::common::import_default_gdt;
use crate::elf::load_static_elf_with_buffer;
use crate::importer::BootPageAcceptance;
use crate::importer::ImageLoad;
use crate::importer::X86Register;
use crate::mptable;
use hvdef::HV_PAGE_SIZE;
use loader_defs::linux as defs;
use memory_range::MemoryRange;
use page_table::IdentityMapSize;
use page_table::x64::IdentityMapBuilder;
use page_table::x64::PageTable;
use page_table::x64::align_up_to_large_page_size;
use page_table::x64::align_up_to_page_size;
use std::ffi::CString;
use std::io::Read;
use std::io::Seek;
use vm_topology::memory::MemoryLayout;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const MPTABLE_CMDLINE_BASE: u64 = 0x2_0000;
const MPTABLE_CMDLINE_END: u64 = 0x3_0000;
const MPTABLE_ISA_HOLE_BASE: u64 = 0xa_0000;
const MPTABLE_ISA_HOLE_END: u64 = 0x10_0000;
const MPTABLE_MMIO_GAP_BASE: u64 = 0xc000_0000;
const MPTABLE_MMIO_GAP_END: u64 = 0x1_0000_0000;
const MPTABLE_LOAD_CHUNK_SIZE: usize = 1024 * 1024;
const MPTABLE_PAGE_TABLE_COUNT: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct E820Range {
    start: u64,
    end: u64,
    entry_type: u32,
}

fn build_setup_header(cmdline: &CString, initrd_base: u32, initrd_size: u32) -> defs::setup_header {
    const LOADER_TYPE_UNREGISTERED: u8 = 0xff;

    let mut hdr = defs::setup_header {
        boot_flag: 0xaa55.into(),
        header: 0x53726448.into(),
        kernel_alignment: 0x100000.into(),
        ..FromZeros::new_zeroed()
    };
    hdr.type_of_loader = LOADER_TYPE_UNREGISTERED;
    hdr.cmd_line_ptr = MPTABLE_CMDLINE_BASE.try_into().expect("must fit in u32");
    hdr.cmdline_size = (cmdline.as_bytes().len() as u64)
        .try_into()
        .expect("must fit in u32");
    hdr.ramdisk_image = initrd_base.into();
    hdr.ramdisk_size = initrd_size.into();
    hdr
}

fn range_is_in_ram(mem_layout: &MemoryLayout, range: MemoryRange) -> bool {
    mem_layout
        .ram()
        .iter()
        .any(|ram| range.start() >= ram.range.start() && range.end() <= ram.range.end())
}

fn build_mptable_e820(
    mem_layout: &MemoryLayout,
    reserved_memory_ranges: &[MemoryRange],
) -> Result<Vec<E820Range>, Error> {
    let mut previous_end = 0;
    for range in reserved_memory_ranges {
        let valid = !range.is_empty()
            && range.start().is_multiple_of(HV_PAGE_SIZE)
            && range.end().is_multiple_of(HV_PAGE_SIZE)
            && range.start() >= previous_end
            && range_is_in_ram(mem_layout, *range);
        if !valid {
            return Err(Error::InvalidReservedMemoryRange {
                start: range.start(),
                end: range.end(),
            });
        }
        previous_end = range.end();
    }

    let isa_hole = MemoryRange::new(MPTABLE_ISA_HOLE_BASE..MPTABLE_ISA_HOLE_END);
    let mmio_gap = MemoryRange::new(MPTABLE_MMIO_GAP_BASE..MPTABLE_MMIO_GAP_END);
    if !range_is_in_ram(mem_layout, isa_hole)
        || mem_layout
            .ram()
            .iter()
            .any(|ram| ram.range.overlaps(&mmio_gap))
        || reserved_memory_ranges
            .iter()
            .any(|range| range.overlaps(&isa_hole))
    {
        return Err(Error::InvalidMpTableMemoryLayout);
    }

    let mut reservations = reserved_memory_ranges.to_vec();
    reservations.push(isa_hole);
    reservations.sort();

    let mut entries = Vec::with_capacity(mem_layout.ram().len() + reservations.len() * 2 + 1);
    for ram in mem_layout.ram() {
        let mut next = ram.range.start();
        for reserved in reservations.iter().filter(|reserved| {
            reserved.start() >= ram.range.start() && reserved.end() <= ram.range.end()
        }) {
            if next < reserved.start() {
                entries.push(E820Range {
                    start: next,
                    end: reserved.start(),
                    entry_type: defs::E820_RAM,
                });
            }
            entries.push(E820Range {
                start: reserved.start(),
                end: reserved.end(),
                entry_type: defs::E820_RESERVED,
            });
            next = reserved.end();
        }
        if next < ram.range.end() {
            entries.push(E820Range {
                start: next,
                end: ram.range.end(),
                entry_type: defs::E820_RAM,
            });
        }
    }
    entries.sort_by_key(|entry| (entry.start, entry.end));

    let mut merged: Vec<E820Range> = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.start >= entry.end {
            return Err(Error::InvalidMpTableMemoryLayout);
        }
        if let Some(previous) = merged.last_mut() {
            if entry.start < previous.end {
                return Err(Error::InvalidMpTableMemoryLayout);
            }
            if entry.start == previous.end && entry.entry_type == previous.entry_type {
                previous.end = entry.end;
                continue;
            }
        }
        merged.push(entry);
    }
    Ok(merged)
}

fn build_mptable_zero_page(
    mem_layout: &MemoryLayout,
    reserved_memory_ranges: &[MemoryRange],
    cmdline: &CString,
    initrd_base: u32,
    initrd_size: u32,
) -> Result<defs::boot_params, Error> {
    let hdr = build_setup_header(cmdline, initrd_base, initrd_size);
    let mut boot_params = defs::boot_params {
        hdr,
        ..FromZeros::new_zeroed()
    };
    let entries = build_mptable_e820(mem_layout, reserved_memory_ranges)?;
    let capacity = boot_params.e820_map.len();
    if entries.len() > capacity {
        return Err(Error::TooManyMemoryRanges(capacity));
    }
    for (output, entry) in boot_params.e820_map.iter_mut().zip(&entries) {
        *output = defs::e820entry {
            addr: entry.start.into(),
            size: (entry.end - entry.start).into(),
            typ: entry.entry_type.into(),
        };
    }
    boot_params.e820_entries =
        u8::try_from(entries.len()).map_err(|_| Error::TooManyMemoryRanges(capacity))?;
    Ok(boot_params)
}

fn import_initrd_with_buffer(
    initrd: Option<InitrdConfig<'_>>,
    next_addr: u64,
    importer: &mut dyn ImageLoad<X86Register>,
    buffer: &mut ChunkBuf,
) -> Result<Option<InitrdInfo>, Error> {
    let initrd_info = match initrd {
        Some(cfg) => {
            let initrd_address = match cfg.initrd_address {
                InitrdAddressType::AfterKernel => align_up_to_large_page_size(next_addr),
                InitrdAddressType::Address(addr) => addr,
            };

            tracing::trace!(initrd_address, "loading initrd");
            super::check_address_alignment(initrd_address)?;

            buffer
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

fn load_uncompressed_kernel_and_initrd_x64_with_buffer<F>(
    importer: &mut dyn ImageLoad<X86Register>,
    kernel_image: &mut F,
    kernel_minimum_start_address: u64,
    initrd: Option<InitrdConfig<'_>>,
    buffer: &mut ChunkBuf,
) -> Result<LoadInfo, Error>
where
    F: Read + Seek,
{
    let elf_load_info = load_static_elf_with_buffer(
        importer,
        kernel_image,
        kernel_minimum_start_address,
        0,
        false,
        BootPageAcceptance::Exclusive,
        "linux-kernel",
        buffer,
    )
    .map_err(Error::ElfLoader)?;

    let crate::elf::LoadInfo {
        minimum_address_used: min_addr,
        next_available_address: next_addr,
        entrypoint,
    } = elf_load_info;
    tracing::trace!(min_addr, next_addr, entrypoint, "loaded kernel");

    let initrd_info = import_initrd_with_buffer(initrd, next_addr, importer, buffer)?;

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

fn import_command_line(
    importer: &mut impl ImageLoad<X86Register>,
    cmdline: &CString,
) -> Result<(), Error> {
    let raw_cmdline = cmdline.as_bytes_with_nul();
    let slot_size = MPTABLE_CMDLINE_END - MPTABLE_CMDLINE_BASE;
    if raw_cmdline.len() as u64 > slot_size {
        return Err(Error::CommandLineTooLong(raw_cmdline.len(), slot_size));
    }
    if raw_cmdline.len() > 1 {
        let cmdline_size_pages = align_up_to_page_size(raw_cmdline.len() as u64) / HV_PAGE_SIZE;
        importer
            .import_pages(
                MPTABLE_CMDLINE_BASE / HV_PAGE_SIZE,
                cmdline_size_pages,
                "linux-commandline",
                BootPageAcceptance::Exclusive,
                raw_cmdline,
            )
            .map_err(Error::Importer)?;
    }
    Ok(())
}

fn import_x86_boot_pages(importer: &mut impl ImageLoad<X86Register>) -> Result<(), Error> {
    import_default_gdt(importer, GDT_BASE / HV_PAGE_SIZE).map_err(Error::Importer)?;
    let mut page_table_work_buffer: Vec<PageTable> =
        vec![PageTable::new_zeroed(); MPTABLE_PAGE_TABLE_COUNT];
    let mut page_table: Vec<u8> = vec![0; MPTABLE_PAGE_TABLE_COUNT * HV_PAGE_SIZE as usize];
    let page_table = IdentityMapBuilder::new(
        CR3_BASE,
        IdentityMapSize::Size4Gb,
        page_table_work_buffer.as_mut_slice(),
        page_table.as_mut_slice(),
    )?
    .build();
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
    Ok(())
}

fn import_x86_registers(
    importer: &mut impl ImageLoad<X86Register>,
    load_info: &LoadInfo,
) -> Result<(), Error> {
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
    import_reg(X86Register::Rip(load_info.kernel.entrypoint))?;
    import_reg(X86Register::Rsi(ZERO_PAGE_BASE))?;
    Ok(())
}

fn import_mptable_config(
    importer: &mut impl ImageLoad<X86Register>,
    load_info: &LoadInfo,
    cmdline: &CString,
    mem_layout: &MemoryLayout,
    config: &mptable::MpTableConfig<'_>,
    reserved_memory_ranges: &[MemoryRange],
) -> Result<(), Error> {
    let raw_cmdline = cmdline.as_bytes_with_nul();
    let slot_size = MPTABLE_CMDLINE_END - MPTABLE_CMDLINE_BASE;
    if raw_cmdline.len() as u64 > slot_size {
        return Err(Error::CommandLineTooLong(raw_cmdline.len(), slot_size));
    }
    let tables = mptable::build(config)?;
    let table_end = mptable::MP_CONFIG_TABLE_ADDR
        .checked_add(tables.configuration_table.len())
        .ok_or(Error::InvalidMpTableMemoryLayout)?;
    if table_end > GDT_BASE as usize {
        return Err(Error::MpTableOverlap {
            table_end,
            gdt_addr: GDT_BASE,
        });
    }
    let boot_params = build_mptable_zero_page(
        mem_layout,
        reserved_memory_ranges,
        cmdline,
        load_info.initrd.as_ref().map(|info| info.gpa).unwrap_or(0) as u32,
        load_info.initrd.as_ref().map(|info| info.size).unwrap_or(0) as u32,
    )?;

    let mut mp_page = [0u8; HV_PAGE_SIZE as usize];
    mp_page[mptable::MP_FLOATING_POINTER_ADDR
        ..mptable::MP_FLOATING_POINTER_ADDR + tables.floating_pointer.len()]
        .copy_from_slice(&tables.floating_pointer);
    mp_page[mptable::MP_CONFIG_TABLE_ADDR..table_end].copy_from_slice(&tables.configuration_table);
    importer
        .import_pages(
            0,
            1,
            "linux-mptable",
            BootPageAcceptance::Exclusive,
            &mp_page,
        )
        .map_err(Error::Importer)?;

    import_command_line(importer, cmdline)?;
    import_x86_boot_pages(importer)?;
    importer
        .import_pages(
            ZERO_PAGE_BASE / HV_PAGE_SIZE,
            1,
            "linux-zeropage",
            BootPageAcceptance::Exclusive,
            boot_params.as_bytes(),
        )
        .map_err(Error::Importer)?;
    import_x86_registers(importer, load_info)?;
    Ok(())
}

/// Loads an uncompressed x86-64 ELF kernel through Linux direct boot with
/// Intel MP tables and no ACPI, SMBIOS, or firmware tables.
pub fn load_x86_mptable<F>(
    importer: &mut impl ImageLoad<X86Register>,
    kernel_image: &mut F,
    initrd: Option<InitrdConfig<'_>>,
    cmdline: &CString,
    mem_layout: &MemoryLayout,
    config: &mptable::MpTableConfig<'_>,
    reserved_memory_ranges: &[MemoryRange],
) -> Result<LoadInfo, Error>
where
    F: Read + Seek,
{
    if crate::bzimage::is_bzimage(kernel_image).map_err(Error::BzImage)? {
        return Err(Error::MpTableRequiresElf);
    }
    let mut buffer = ChunkBuf::with_size(MPTABLE_LOAD_CHUNK_SIZE);
    let load_info = load_uncompressed_kernel_and_initrd_x64_with_buffer(
        importer,
        kernel_image,
        KERNEL_BASE,
        initrd,
        &mut buffer,
    )?;
    import_mptable_config(
        importer,
        &load_info,
        cmdline,
        mem_layout,
        config,
        reserved_memory_ranges,
    )?;
    Ok(load_info)
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

    fn make_mptable_layout(ram_size: u64) -> MemoryLayout {
        MemoryLayout::new(
            ram_size,
            &[MemoryRange::new(3 * GB..4 * GB)],
            &[],
            &[],
            None,
        )
        .unwrap()
    }

    #[derive(Default)]
    struct RecordingImporter {
        pages: Vec<(String, u64, u64)>,
        imports: Vec<ImportRecord>,
        registers: Vec<X86Register>,
        vp_context_page: Option<u64>,
    }

    #[derive(Debug)]
    struct ImportRecord {
        page_base: u64,
        page_count: u64,
        tag: String,
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
            _acceptance: BootPageAcceptance,
            data: &[u8],
        ) -> anyhow::Result<()> {
            self.pages
                .push((debug_tag.to_string(), page_base, page_count));
            self.imports.push(ImportRecord {
                page_base,
                page_count,
                tag: debug_tag.to_string(),
                data: data.to_vec(),
            });
            Ok(())
        }

        fn import_vp_register(&mut self, register: X86Register) -> anyhow::Result<()> {
            self.registers.push(register);
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

        fn set_vp_context_page(&mut self, page_base: u64) -> anyhow::Result<()> {
            self.vp_context_page = Some(page_base);
            Ok(())
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

    fn mptable_config(apic_ids: &[u32]) -> mptable::MpTableConfig<'_> {
        mptable::MpTableConfig {
            apic_ids,
            level_triggered_irqs: &[4, 5, 6, 7],
        }
    }

    fn imported_zero_page(importer: &RecordingImporter) -> defs::boot_params {
        let data = &importer
            .imports
            .iter()
            .find(|import| import.tag == "linux-zeropage")
            .unwrap()
            .data;
        defs::boot_params::read_from_bytes(data).unwrap()
    }

    #[test]
    fn mptable_config_places_boot_data_without_firmware_tables() {
        let apic_ids = [0, 1, 2, 3];
        let mut importer = RecordingImporter::default();
        import_mptable_config(
            &mut importer,
            &test_load_info(),
            &CString::new("console=hvc0").unwrap(),
            &make_layout(256 * MB),
            &mptable_config(&apic_ids),
            &[MemoryRange::new(0x3_0000..0x3_1000)],
        )
        .unwrap();

        let mp = importer
            .imports
            .iter()
            .find(|import| import.tag == "linux-mptable")
            .unwrap();
        assert_eq!(mp.page_base, 0);
        assert_eq!(mp.page_count, 1);
        assert_eq!(
            &mp.data[mptable::MP_FLOATING_POINTER_ADDR..mptable::MP_FLOATING_POINTER_ADDR + 4],
            b"_MP_"
        );
        let table_length = u16::from_le_bytes(
            mp.data[mptable::MP_CONFIG_TABLE_ADDR + 4..mptable::MP_CONFIG_TABLE_ADDR + 6]
                .try_into()
                .unwrap(),
        ) as usize;
        assert!(mptable::MP_CONFIG_TABLE_ADDR + table_length <= GDT_BASE as usize);
        assert_eq!(
            mp.data[mptable::MP_CONFIG_TABLE_ADDR..mptable::MP_CONFIG_TABLE_ADDR + table_length]
                .iter()
                .copied()
                .fold(0u8, |sum, byte| sum.wrapping_add(byte)),
            0
        );
        assert_eq!(
            importer.page_base("linux-commandline"),
            Some(MPTABLE_CMDLINE_BASE / HV_PAGE_SIZE)
        );
        assert!(importer.imports.iter().all(|import| {
            !import.tag.contains("acpi")
                && !import.tag.contains("rsdp")
                && !import.tag.contains("smbios")
        }));

        let boot_params = imported_zero_page(&importer);
        assert_eq!(boot_params.acpi_rsdp_addr, 0);
        assert_eq!(
            u32::from(boot_params.hdr.cmd_line_ptr),
            MPTABLE_CMDLINE_BASE as u32
        );
        assert_eq!(u32::from(boot_params.hdr.cmdline_size), 12);
        assert!(
            importer
                .registers
                .contains(&X86Register::Rsi(ZERO_PAGE_BASE))
        );
        assert!(importer.registers.contains(&X86Register::Cr3(CR3_BASE)));
        assert!(
            importer
                .registers
                .contains(&X86Register::Cr0(x86defs::X64_CR0_PG | x86defs::X64_CR0_PE))
        );
        assert!(
            importer
                .registers
                .contains(&X86Register::Cr4(x86defs::X64_CR4_PAE))
        );
        assert!(importer.registers.contains(&X86Register::Rip(KERNEL_BASE)));
        assert!(
            !importer
                .registers
                .contains(&X86Register::Pat(x86defs::X86X_MSR_DEFAULT_PAT))
        );
        assert!(
            !importer
                .registers
                .contains(&X86Register::MtrrDefType(0xc00))
        );
    }

    #[test]
    fn mptable_config_supports_full_command_line_region() {
        let cmdline = CString::new(vec![b'a'; 65_535]).unwrap();
        let mut importer = RecordingImporter::default();
        import_mptable_config(
            &mut importer,
            &test_load_info(),
            &cmdline,
            &make_layout(256 * MB),
            &mptable_config(&[0]),
            &[MemoryRange::new(0x3_0000..0x3_1000)],
        )
        .unwrap();
        let imported = importer
            .imports
            .iter()
            .find(|import| import.tag == "linux-commandline")
            .unwrap();
        assert_eq!(imported.page_base, MPTABLE_CMDLINE_BASE / HV_PAGE_SIZE);
        assert_eq!(imported.page_count, 16);
        assert_eq!(imported.data.len(), 65_536);
        assert_eq!(imported.data[65_535], 0);

        let mut importer = RecordingImporter::default();
        let cmdline = CString::new(vec![b'a'; 65_536]).unwrap();
        let error = import_mptable_config(
            &mut importer,
            &test_load_info(),
            &cmdline,
            &make_layout(256 * MB),
            &mptable_config(&[0]),
            &[MemoryRange::new(0x3_0000..0x3_1000)],
        )
        .unwrap_err();
        assert!(matches!(error, Error::CommandLineTooLong(65_537, 65_536)));
        assert!(importer.imports.is_empty());
    }

    #[test]
    fn mptable_e820_reserves_status_and_isa_hole_and_leaves_mmio_gap() {
        let entries = build_mptable_e820(
            &make_mptable_layout(8 * GB),
            &[MemoryRange::new(0x3_0000..0x3_1000)],
        )
        .unwrap();
        assert_eq!(
            entries.as_slice(),
            &[
                E820Range {
                    start: 0,
                    end: 0x3_0000,
                    entry_type: defs::E820_RAM,
                },
                E820Range {
                    start: 0x3_0000,
                    end: 0x3_1000,
                    entry_type: defs::E820_RESERVED,
                },
                E820Range {
                    start: 0x3_1000,
                    end: MPTABLE_ISA_HOLE_BASE,
                    entry_type: defs::E820_RAM,
                },
                E820Range {
                    start: MPTABLE_ISA_HOLE_BASE,
                    end: MPTABLE_ISA_HOLE_END,
                    entry_type: defs::E820_RESERVED,
                },
                E820Range {
                    start: MPTABLE_ISA_HOLE_END,
                    end: MPTABLE_MMIO_GAP_BASE,
                    entry_type: defs::E820_RAM,
                },
                E820Range {
                    start: MPTABLE_MMIO_GAP_END,
                    end: 9 * GB,
                    entry_type: defs::E820_RAM,
                },
            ]
        );
    }

    #[test]
    fn mptable_e820_rejects_invalid_reservations_and_capacity() {
        let layout = make_mptable_layout(256 * MB);
        for ranges in [
            vec![MemoryRange::new(256 * MB..256 * MB + HV_PAGE_SIZE)],
            vec![
                MemoryRange::new(0x4_0000..0x4_2000),
                MemoryRange::new(0x4_1000..0x4_3000),
            ],
            vec![
                MemoryRange::new(0x5_0000..0x5_1000),
                MemoryRange::new(0x4_0000..0x4_1000),
            ],
        ] {
            assert!(matches!(
                build_mptable_e820(&layout, &ranges),
                Err(Error::InvalidReservedMemoryRange { .. })
            ));
        }

        let reservations = (0..70)
            .map(|index| {
                let start = 0x1_0000 + index * 0x2000;
                MemoryRange::new(start..start + HV_PAGE_SIZE)
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            build_mptable_zero_page(&layout, &reservations, &CString::new("").unwrap(), 0, 0,),
            Err(Error::TooManyMemoryRanges(128))
        ));
    }

    #[test]
    fn mptable_config_rejects_table_overlap_before_import() {
        let apic_ids = (0..200).collect::<Vec<_>>();
        let mut importer = RecordingImporter::default();
        let error = import_mptable_config(
            &mut importer,
            &test_load_info(),
            &CString::new("").unwrap(),
            &make_mptable_layout(256 * MB),
            &mptable_config(&apic_ids),
            &[MemoryRange::new(0x3_0000..0x3_1000)],
        )
        .unwrap_err();
        assert!(matches!(error, Error::MpTableOverlap { .. }));
        assert!(importer.imports.is_empty());
    }
}
