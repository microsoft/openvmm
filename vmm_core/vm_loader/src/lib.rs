// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Loader glue code shared between both OpenVMM and Underhill.
//!
//! DEVNOTE: this organization isn't great, and should be reconsidered...

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use anyhow::Context;
use guestmem::GuestMemory;
use hvdef::HV_PAGE_SIZE;
use hvdef::Vtl;
use loader::importer::BootPageAcceptance;
use loader::importer::GuestArch;
use loader::importer::ImageLoad;
use loader::importer::StartupMemoryType;
use memory_range::MemoryRange;
use range_map_vec::Entry;
use range_map_vec::RangeMap;
use std::collections::HashMap;
use std::fmt::Debug;
use std::mem::Discriminant;
use virt::PageVisibility;
use vm_topology::memory::MemoryLayout;

pub mod initial_regs;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RangeInfo {
    tag: String,
    acceptance: BootPageAcceptance,
}

#[derive(Debug)]
pub struct Loader<'a, R> {
    gm: GuestMemory,
    regs: HashMap<Discriminant<R>, R>,
    mem_layout: &'a MemoryLayout,
    accepted_ranges: RangeMap<u64, RangeInfo>,
    max_vtl: Vtl,
}

impl<R> Loader<'_, R> {
    pub fn new(gm: GuestMemory, mem_layout: &MemoryLayout, max_vtl: Vtl) -> Loader<'_, R> {
        Loader {
            gm,
            regs: HashMap::new(),
            mem_layout,
            accepted_ranges: RangeMap::new(),
            max_vtl,
        }
    }

    pub fn initial_regs(self) -> Vec<R> {
        self.regs.into_values().collect()
    }

    pub fn initial_regs_and_accepted_ranges(
        mut self,
    ) -> (Vec<R>, Vec<(MemoryRange, PageVisibility)>) {
        let regs = self.regs.into_values().collect();

        // Merge adjacent ranges first to help cut down on the number of entries
        // in the initial acceptance list. Since we load from an IGVM file, most
        // ranges are a single 4K page which can be merged for easier viewing.
        self.accepted_ranges
            .merge_adjacent(range_map_vec::u64_is_adjacent);

        let pages = self
            .accepted_ranges
            .into_vec()
            .iter()
            .map(|(start, end, info)| {
                let range = MemoryRange::from_4k_gpn_range(*start..(*end + 1));
                let vis = match info.acceptance {
                    BootPageAcceptance::Exclusive => PageVisibility::Exclusive,
                    BootPageAcceptance::ExclusiveUnmeasured => PageVisibility::Exclusive,
                    BootPageAcceptance::Shared => PageVisibility::Shared,
                    // TODO: These are required for hardware isolation but
                    // support for that doesn't exist in any virt backend yet.
                    // Handling these will require more virt::generic types.
                    BootPageAcceptance::VpContext => todo!(),
                    BootPageAcceptance::ErrorPage => todo!(),
                    BootPageAcceptance::SecretsPage => todo!(),
                    BootPageAcceptance::CpuidPage => todo!(),
                    BootPageAcceptance::CpuidExtendedStatePage => todo!(),
                };
                (range, vis)
            })
            .collect();

        (regs, pages)
    }

    /// Accept a new page range with a given acceptance into the map of accepted ranges.
    pub fn accept_new_range(
        &mut self,
        page_base: u64,
        page_count: u64,
        tag: &str,
        acceptance: BootPageAcceptance,
    ) -> anyhow::Result<()> {
        let page_end = page_base + page_count - 1;
        match self.accepted_ranges.entry(page_base..=page_end) {
            Entry::Overlapping(entry) => {
                let (overlap_start, overlap_end, ref overlap_info) = *entry.get();
                Err(anyhow::anyhow!(
                    "{} at {} ({:?}) overlaps {} at {}",
                    tag,
                    MemoryRange::from_4k_gpn_range(page_base..page_end + 1),
                    acceptance,
                    overlap_info.tag,
                    MemoryRange::from_4k_gpn_range(overlap_start..overlap_end + 1),
                ))
            }
            Entry::Vacant(entry) => {
                entry.insert(RangeInfo {
                    tag: tag.to_string(),
                    acceptance,
                });
                Ok(())
            }
        }
    }
}

impl<R: Debug + GuestArch> ImageLoad<R> for Loader<'_, R> {
    fn isolation_config(&self) -> loader::importer::IsolationConfig {
        // For now, all OpenVMM VMs are non-isolated.
        loader::importer::IsolationConfig {
            paravisor_present: false,
            isolation_type: loader::importer::IsolationType::None,
            shared_gpa_boundary_bits: None,
        }
    }

    fn import_pages(
        &mut self,
        page_base: u64,
        page_count: u64,
        debug_tag: &str,
        acceptance: BootPageAcceptance,
        data: &[u8],
    ) -> anyhow::Result<()> {
        tracing::trace!(
            page_base,
            page_count,
            import_len = page_count * HV_PAGE_SIZE,
            data_len = data.len(),
            ?acceptance,
            "importing pages"
        );

        // Track accepted ranges for duplicate imports.
        self.accept_new_range(page_base, page_count, debug_tag, acceptance)?;

        // Page count must be larger or equal to data.
        let size_bytes = (page_count * HV_PAGE_SIZE) as usize;
        let base_addr = page_base * HV_PAGE_SIZE;
        if size_bytes < data.len() {
            anyhow::bail!(
                "data {:x} larger than supplied page count {:x}",
                data.len(),
                page_count
            );
        }

        // Write the contained data.
        self.gm
            .write_at(base_addr, data)
            .context("unable to import data")?;

        // Remaining bytes must be zeroed.
        let remaining = size_bytes - data.len();
        self.gm
            .fill_at(base_addr + data.len() as u64, 0, remaining)
            .context("unable to zero remaining import")
    }

    fn import_vp_register(&mut self, register: R) -> anyhow::Result<()> {
        let entry = self.regs.entry(std::mem::discriminant(&register));
        match entry {
            std::collections::hash_map::Entry::Occupied(_) => {
                panic!("duplicate register import {:?}", register)
            }
            std::collections::hash_map::Entry::Vacant(ve) => ve.insert(register),
        };

        Ok(())
    }

    fn verify_startup_memory_available(
        &mut self,
        page_base: u64,
        page_count: u64,
        memory_type: StartupMemoryType,
    ) -> anyhow::Result<()> {
        // Allow Vtl2ProtectableRam only if VTL2 is enabled.
        if self.max_vtl == Vtl::Vtl2 {
            match memory_type {
                StartupMemoryType::Ram => {}
                StartupMemoryType::Vtl2ProtectableRam => {
                    // TODO: Should enable VTl2 memory protections on this region? Or do we allow VTL2 memory protections
                    //       on the whole address space when VTL memory protections work?
                    tracing::warn!(page_base, page_count, "vtl2 protectable ram requested");
                }
            }
        } else if memory_type != StartupMemoryType::Ram {
            anyhow::bail!("memory type {memory_type:?} not available");
        }

        let mut memory_found = false;

        let base_address = page_base * HV_PAGE_SIZE;
        let end_address = base_address + (page_count * HV_PAGE_SIZE) - 1;

        for range in self.mem_layout.ram() {
            if base_address >= range.range.start() && base_address < range.range.end() {
                // Today, the memory layout only describes normal ram and mmio.
                // Thus the memory request must live completely within a single
                // range, since any gaps are mmio.
                if end_address > range.range.end() {
                    anyhow::bail!(
                        "requested memory at base {:#x} and end {:#x} is not covered fully by the corresponding range {:?}",
                        base_address,
                        end_address,
                        range
                    );
                }

                memory_found = true;
            }
        }

        // TODO: It seems very weird to check both ram and this vtl2 range.
        // seems like vtl2 absolute addr should maybe carve the vtl2 range out
        // of mem_layout? but that has its own issues
        //
        // Memory might be described as a VTL2 specific range. Only check this
        // if we haven't found the range, and this is for VTL2.
        if !memory_found && memory_type == StartupMemoryType::Vtl2ProtectableRam {
            if let Some(range) = self.mem_layout.vtl2_range() {
                if base_address >= range.start() && (page_count * HV_PAGE_SIZE) <= range.len() {
                    memory_found = true;
                } else {
                    anyhow::bail!(
                        "startup vtl2 memory at base {:#x} and end {:#x} is not covered fully by vtl2 specific ram range {:?}",
                        base_address,
                        end_address,
                        range
                    );
                }
            }
        }

        if memory_found {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "no valid memory range available for memory at base {:#x} end {:#x}",
                base_address,
                end_address
            ))
        }
    }

    fn set_vp_context_page(&mut self, _page_base: u64) -> anyhow::Result<()> {
        unimplemented!()
    }

    fn create_parameter_area(
        &mut self,
        _page_base: u64,
        _page_count: u32,
        _debug_tag: &str,
    ) -> anyhow::Result<loader::importer::ParameterAreaIndex> {
        unimplemented!()
    }

    fn create_parameter_area_with_data(
        &mut self,
        _page_base: u64,
        _page_count: u32,
        _debug_tag: &str,
        _initial_data: &[u8],
    ) -> anyhow::Result<loader::importer::ParameterAreaIndex> {
        unimplemented!()
    }

    fn import_parameter(
        &mut self,
        _parameter_area: loader::importer::ParameterAreaIndex,
        _byte_offset: u32,
        _parameter_type: loader::importer::IgvmParameterType,
    ) -> anyhow::Result<()> {
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
