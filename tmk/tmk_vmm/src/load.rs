// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for loading a TMK into VM memory.

use anyhow::Context as _;
use fs_err::File;
use guestmem::GuestMemory;
use hvdef::Vtl;
use loader::importer::GuestArch;
use loader::importer::ImageLoad;
use loader::importer::X86Register;
use object::Endianness;
use object::Object;
use object::ObjectSection;
use object::ObjectSegment as _;
use std::fmt::Debug;
use std::sync::Arc;
use virt::VpIndex;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::aarch64::Aarch64Topology;
use vm_topology::processor::x86::X86Topology;
use zerocopy::FromBytes as _;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Loads a TMK, returning the initial registers for the BSP.
#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
pub fn load_x86(
    memory_layout: &MemoryLayout,
    guest_memory: &GuestMemory,
    processor_topology: &ProcessorTopology<X86Topology>,
    caps: &virt::x86::X86PartitionCapabilities,
    tmk: &File,
    test: &TestInfo,
) -> anyhow::Result<Arc<virt::x86::X86InitialRegs>> {
    let mut loader = vm_loader::Loader::new(guest_memory.clone(), memory_layout, Vtl::Vtl0);
    let load_info = load_common(&mut loader, tmk, test)?;

    let page_table_base = load_info.next_available_address;
    let mut page_table_work_buffer: Vec<page_table::x64::PageTable> =
        vec![page_table::x64::PageTable::new_zeroed(); page_table::x64::PAGE_TABLE_MAX_COUNT];
    let mut page_tables: Vec<u8> = vec![0; page_table::x64::PAGE_TABLE_MAX_BYTES];
    let page_table_builder = page_table::x64::IdentityMapBuilder::new(
        page_table_base,
        page_table::IdentityMapSize::Size4Gb,
        page_table_work_buffer.as_mut_slice(),
        page_tables.as_mut_slice(),
    )?;
    let page_tables = page_table_builder.build();
    loader
        .import_pages(
            page_table_base >> 12,
            page_tables.len() as u64 >> 12,
            "page_tables",
            loader::importer::BootPageAcceptance::Exclusive,
            page_tables,
        )
        .context("failed to import page tables")?;

    let gdt_base = page_table_base + page_tables.len() as u64;
    loader::common::import_default_gdt(&mut loader, gdt_base >> 12)
        .context("failed to import gdt")?;

    let mut import_reg = |reg| {
        loader
            .import_vp_register(reg)
            .context("failed to set register")
    };
    import_reg(X86Register::Cr0(x86defs::X64_CR0_PG | x86defs::X64_CR0_PE))?;
    import_reg(X86Register::Cr3(page_table_base))?;
    import_reg(X86Register::Cr4(x86defs::X64_CR4_PAE))?;
    import_reg(X86Register::Efer(
        x86defs::X64_EFER_SCE
            | x86defs::X64_EFER_LME
            | x86defs::X64_EFER_LMA
            | x86defs::X64_EFER_NXE,
    ))?;
    import_reg(X86Register::Rip(load_info.entrypoint))?;
    import_reg(X86Register::Rsi(load_info.param))?;

    let regs = vm_loader::initial_regs::x86_initial_regs(
        &loader.initial_regs(),
        caps,
        &processor_topology.vp_arch(VpIndex::BSP),
    );
    Ok(regs)
}

#[cfg_attr(not(guest_arch = "aarch64"), expect(dead_code))]
pub fn load_aarch64(
    memory_layout: &MemoryLayout,
    guest_memory: &GuestMemory,
    processor_topology: &ProcessorTopology<Aarch64Topology>,
    caps: &virt::aarch64::Aarch64PartitionCapabilities,
    tmk: &File,
    test: &TestInfo,
) -> anyhow::Result<Arc<virt::aarch64::Aarch64InitialRegs>> {
    let mut loader = vm_loader::Loader::new(guest_memory.clone(), memory_layout, Vtl::Vtl0);
    let load_info = load_common(&mut loader, tmk, test)?;

    let mut import_reg = |reg| {
        loader
            .import_vp_register(reg)
            .context("failed to set register")
    };

    import_reg(loader::importer::Aarch64Register::Pc(load_info.entrypoint))?;
    import_reg(loader::importer::Aarch64Register::X0(load_info.param))?;
    let regs = vm_loader::initial_regs::aarch64_initial_regs(
        &loader.initial_regs(),
        caps,
        &processor_topology.vp_arch(VpIndex::BSP),
    );

    Ok(regs)
}

fn load_common<R: Debug + GuestArch>(
    loader: &mut vm_loader::Loader<'_, R>,
    tmk: &File,
    test: &TestInfo,
) -> anyhow::Result<LoadInfo> {
    let load_info = loader::elf::load_static_elf(
        loader,
        &mut &*tmk,
        0,
        0x200000,
        false,
        loader::importer::BootPageAcceptance::Exclusive,
        "tmk",
    )
    .context("failed to load tmk")?;

    let start_input = tmk_protocol::StartInput {
        command: crate::run::COMMAND_ADDRESS,
        test_index: test.index,
    };

    let start_input_addr = load_info.next_available_address;

    loader.import_pages(
        start_input_addr >> 12,
        1,
        "start_input",
        loader::importer::BootPageAcceptance::Exclusive,
        start_input.as_bytes(),
    )?;

    Ok(LoadInfo {
        entrypoint: load_info.entrypoint,
        param: start_input_addr,
        next_available_address: start_input_addr + 0x1000,
    })
}

struct LoadInfo {
    entrypoint: u64,
    param: u64,
    next_available_address: u64,
}

#[derive(Clone)]
pub struct TestInfo {
    pub name: String,
    pub index: u64,
}

/// Enumerate the tests from a TMK binary.
///
/// The test definitions are stored as an array of
/// [`tmk_protocol::TestDescriptor64`] in the "tmk_tests" section of the binary.
pub fn enumerate_tests(tmk: &File) -> anyhow::Result<Vec<TestInfo>> {
    let reader = object::ReadCache::new(tmk);
    let file: object::read::elf::ElfFile64<'_, Endianness, _> =
        object::read::elf::ElfFile::parse(&reader).context("failed to parse TMK")?;

    let mut relocs = file
        .dynamic_relocations()
        .context("failed to find dynamic relocations")?
        .collect::<Vec<_>>();
    relocs.sort_by_key(|&(a, _)| a);

    // Relocate address `v` that was loaded from address `addr`.
    let reloc = |addr, v: u64| {
        let r = relocs.binary_search_by_key(&addr, |&(a, _)| a);
        match r {
            Ok(i) => {
                let reloc = &relocs[i].1;
                v.wrapping_add_signed(reloc.addend())
            }
            Err(_) => v,
        }
    };

    let section = file
        .section_by_name("tmk_tests")
        .context("failed to find tmk_tests section")?;
    let data = section.data()?;
    let descriptors = <[tmk_protocol::TestDescriptor64]>::ref_from_bytes(data)
        .ok()
        .context("failed to parse tmk_tests section")?;
    let mut tests = Vec::with_capacity(descriptors.len());
    for (i, t) in descriptors.iter().enumerate() {
        let name_address = reloc(
            section.address() + (i * size_of::<tmk_protocol::TestDescriptor64>()) as u64,
            t.name,
        );
        let name = file
            .segments()
            .find_map(|s| s.data_range(name_address, t.name_len).transpose())
            .context("failed to find name for test")?
            .context("failed to parse tmk")?;
        let name = core::str::from_utf8(name).context("failed to parse test name")?;

        tests.push(TestInfo {
            name: name.to_string(),
            index: i as u64,
        });
    }

    Ok(tests)
}
