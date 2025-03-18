// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for loading a TMK into VM memory.

use anyhow::Context as _;
use fs_err::File;
use guestmem::GuestMemory;
use hvdef::Vtl;
use loader::importer::ImageLoad;
use loader::importer::X86Register;
use std::sync::Arc;
use virt::VpIndex;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::x86::X86Topology;

/// Loads a TMK, returning the initial registers for the BSP.
#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
pub fn load_x86(
    memory_layout: &MemoryLayout,
    guest_memory: &GuestMemory,
    processor_topology: &ProcessorTopology<X86Topology>,
    caps: &virt::x86::X86PartitionCapabilities,
    tmk: &File,
) -> anyhow::Result<Arc<virt::x86::X86InitialRegs>> {
    let mut loader = vm_loader::Loader::new(guest_memory.clone(), memory_layout, Vtl::Vtl0);
    let load_info = loader::elf::load_static_elf(
        &mut loader,
        &mut &*tmk,
        0,
        0x200000,
        false,
        loader::importer::BootPageAcceptance::Exclusive,
        "tmk",
    )
    .context("failed to load tmk")?;

    let page_table_base = load_info.next_available_address;
    let page_tables = page_table::x64::build_page_tables_64(
        page_table_base,
        0,
        page_table::IdentityMapSize::Size4Gb,
        None,
    );
    loader
        .import_pages(
            page_table_base >> 12,
            page_tables.len() as u64 >> 12,
            "page_tables",
            loader::importer::BootPageAcceptance::Exclusive,
            &page_tables,
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
    import_reg(X86Register::Pat(x86defs::X86X_MSR_DEFAULT_PAT))?;
    import_reg(X86Register::Rip(load_info.entrypoint))?;

    let regs = vm_loader::initial_regs::x86_initial_regs(
        &loader.initial_regs(),
        caps,
        &processor_topology.vp_arch(VpIndex::BSP),
    );
    Ok(regs)
}
