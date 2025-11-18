// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Routines to prepare VTL2 memory for launching the kernel.

use super::address_space::LocalMap;
use super::address_space::init_local_map;
use crate::AddressSpaceManager;
use crate::ShimParams;
use crate::arch::TdxHypercallPage;
use crate::arch::x86_64::address_space::tdx_share_large_page;
use crate::host_params::PartitionInfo;
use crate::host_params::shim_params::IsolationType;
use crate::hypercall::hvcall;
use crate::memory::AllocationPolicy;
use crate::memory::AllocationType;
use crate::off_stack;
use arrayvec::ArrayVec;
use loader_defs::shim::MemoryVtlType;
use memory_range::MemoryRange;
use page_table::x64::MappedRange;
use page_table::x64::PAGE_TABLE_MAX_BYTES;
use page_table::x64::PAGE_TABLE_MAX_COUNT;
use page_table::x64::PageTable;
use page_table::x64::PageTableBuilder;
use sha2::Digest;
use sha2::Sha384;
use static_assertions::const_assert;
use x86defs::X64_LARGE_PAGE_SIZE;
use x86defs::tdx::TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT;
use zerocopy::FromZeros;

/// On isolated systems, transitions all VTL2 RAM to be private and accepted, with the appropriate
/// VTL permissions applied.
pub fn setup_vtl2_memory(
    shim_params: &ShimParams,
    partition_info: &PartitionInfo,
    address_space: &mut AddressSpaceManager,
) {
    // Only if the partition is VBS-isolated, accept memory and apply vtl 2 protections here.
    // Non-isolated partitions can undergo servicing, and additional information
    // would be needed to determine whether vtl 2 protections should be applied
    // or skipped, since the operation is expensive.
    // TODO: if applying vtl 2 protections for non-isolated VMs moves to the
    // boot shim, apply them here.
    if let IsolationType::None = shim_params.isolation_type {
        return;
    }

    if let IsolationType::Vbs = shim_params.isolation_type {
        // Enable VTL protection so that vtl 2 protections can be applied. All other config
        // should be set by the user mode
        let vsm_config = hvdef::HvRegisterVsmPartitionConfig::new()
            .with_default_vtl_protection_mask(0xF)
            .with_enable_vtl_protection(true);

        hvcall()
            .set_register(
                hvdef::HvX64RegisterName::VsmPartitionConfig.into(),
                hvdef::HvRegisterValue::from(u64::from(vsm_config)),
            )
            .expect("setting vsm config shouldn't fail");

        // VBS isolated VMs need to apply VTL2 protections to pages that were already accepted to
        // prevent VTL0 access. Only those pages that belong to the VTL2 RAM region should have
        // these protections applied - certain pages belonging to VTL0 are also among the accepted
        // regions and should not be processed here.
        let accepted_ranges =
            shim_params
                .imported_regions()
                .filter_map(|(imported_range, already_accepted)| {
                    already_accepted.then_some(imported_range)
                });
        for range in memory_range::overlapping_ranges(
            partition_info.vtl2_ram.iter().map(|entry| entry.range),
            accepted_ranges,
        ) {
            hvcall()
                .apply_vtl2_protections(range)
                .expect("applying vtl 2 protections cannot fail");
        }
    }

    // Initialize the local_map
    // TODO: Consider moving this to ShimParams to pass around.
    let mut local_map = match shim_params.isolation_type {
        IsolationType::Snp | IsolationType::Tdx => Some(init_local_map(
            loader_defs::paravisor::PARAVISOR_LOCAL_MAP_VA,
        )),
        IsolationType::None | IsolationType::Vbs => None,
    };

    // Make sure imported regions are in increasing order.
    let mut last_range_end = None;
    for (imported_range, _) in shim_params.imported_regions() {
        assert!(last_range_end.is_none() || imported_range.start() > last_range_end.unwrap());
        last_range_end = Some(imported_range.end() - hvdef::HV_PAGE_SIZE);
    }

    // Iterate over all VTL2 RAM that is not part of an imported region and
    // accept it with appropriate VTL protections.
    for range in memory_range::subtract_ranges(
        partition_info.vtl2_ram.iter().map(|e| e.range),
        shim_params.imported_regions().map(|(r, _)| r),
    ) {
        accept_vtl2_memory(shim_params, &mut local_map, range);
    }

    let ram_buffer = if let Some(bounce_buffer) = shim_params.bounce_buffer {
        assert!(bounce_buffer.start() % X64_LARGE_PAGE_SIZE == 0);
        assert!(bounce_buffer.len() >= X64_LARGE_PAGE_SIZE);

        for range in memory_range::subtract_ranges(
            core::iter::once(bounce_buffer),
            partition_info.vtl2_ram.iter().map(|e| e.range),
        ) {
            accept_vtl2_memory(shim_params, &mut local_map, range);
        }

        // SAFETY: The bounce buffer is trusted as it is obtained from measured
        // shim parameters. The bootloader is identity mapped, and the PA is
        // guaranteed to be mapped as the pagetable is prebuilt and measured.
        unsafe {
            core::slice::from_raw_parts_mut(
                bounce_buffer.start() as *mut u8,
                bounce_buffer.len() as usize,
            )
        }
    } else {
        &mut []
    };

    // Iterate over all imported regions that are not already accepted. They must be accepted here.
    // TODO: No VTL0 memory is currently marked as pending.
    for (imported_range, already_accepted) in shim_params.imported_regions() {
        if !already_accepted {
            accept_pending_vtl2_memory(shim_params, &mut local_map, ram_buffer, imported_range);
        }
    }

    // TDX has specific memory initialization logic. Create a set of page tables for the APs
    // to use during the mailbox spinloop, and carve out memory for TDCALL based hypercalls
    if shim_params.isolation_type == IsolationType::Tdx {
        // Allocate a range of memory for AP page tables
        let page_table_region = address_space
            .allocate_aligned(
                None,
                PAGE_TABLE_MAX_BYTES as u64,
                AllocationType::TdxPageTables,
                AllocationPolicy::LowMemory,
                X64_LARGE_PAGE_SIZE,
            )
            .expect("allocation of space for TDX page tables must succeed");

        // The local map will map a single 2MB PTE per allocation
        const_assert!((PAGE_TABLE_MAX_BYTES as u64) < X64_LARGE_PAGE_SIZE);
        assert_eq!(page_table_region.range.start() % X64_LARGE_PAGE_SIZE, 0);

        let mut local_map = local_map.expect("must be present on TDX");
        let page_table_region_mapping = local_map.map_pages(page_table_region.range, false);
        page_table_region_mapping.data.fill(0);

        const MAX_RANGE_COUNT: usize = 64;
        let mut ranges = off_stack!(
            ArrayVec::<MappedRange, MAX_RANGE_COUNT>,
            ArrayVec::new_const()
        );

        // All VTL2_RAM ranges should be present as R+X in the AP page table mappings, the mailbox
        // wakeup vector will be somewhere in this range, below the 4GB boundary
        const AP_MEMORY_BOUNDARY: u64 = 4 * 1024 * 1024 * 1024;
        let vtl2_ram = address_space
            .vtl2_ranges()
            .filter_map(|(range, typ)| match typ {
                MemoryVtlType::VTL2_RAM => {
                    if range.start() < AP_MEMORY_BOUNDARY {
                        let end = if range.end() < AP_MEMORY_BOUNDARY {
                            range.end()
                        } else {
                            AP_MEMORY_BOUNDARY
                        };
                        Some(MappedRange::new(range.start(), end).read_only())
                    } else {
                        None
                    }
                }
                _ => None,
            });

        ranges.extend(vtl2_ram);

        // Map the reset vector as executable and writable, as the mailbox protocol uses offsets
        // in the reset vector to communicate with the kernel
        const PAGE_SIZE: u64 = 0x1000;
        ranges.push(MappedRange::new(
            x86defs::tdx::RESET_VECTOR_PAGE,
            x86defs::tdx::RESET_VECTOR_PAGE + PAGE_SIZE,
        ));

        ranges.sort_by_key(|r| r.start());

        let mut page_table_work_buffer =
            off_stack!(ArrayVec<PageTable, PAGE_TABLE_MAX_COUNT>, ArrayVec::new_const());
        for _ in 0..PAGE_TABLE_MAX_COUNT {
            page_table_work_buffer.push(PageTable::new_zeroed());
        }

        PageTableBuilder::new(
            page_table_region.range.start(),
            page_table_work_buffer.as_mut_slice(),
            page_table_region_mapping.data,
            ranges.as_slice(),
        )
        .expect("page table builder must return no error")
        .build()
        .expect("page table construction must succeed");

        crate::arch::tdx::tdx_prepare_ap_trampoline(page_table_region.range.start());

        // For TDVMCALL based hypercalls, take the first 2 MB region from ram_buffer for
        // hypercall IO pages. ram_buffer must not be used again beyond this point
        // TODO: find an approach that does not require re-using the ram_buffer
        let free_buffer = ram_buffer.as_mut_ptr() as u64;
        assert!(free_buffer.is_multiple_of(X64_LARGE_PAGE_SIZE));
        // SAFETY: The bottom 2MB region of the ram_buffer is unused by the shim
        // The region is aligned to 2MB, and mapped as a large page
        let tdx_io_page = unsafe {
            tdx_share_large_page(free_buffer);
            TdxHypercallPage::new(free_buffer)
        };
        hvcall().initialize_tdx(tdx_io_page);
    }
}

/// Accepts VTL2 memory in the specified gpa range.
fn accept_vtl2_memory(
    shim_params: &ShimParams,
    local_map: &mut Option<LocalMap<'_>>,
    range: MemoryRange,
) {
    match shim_params.isolation_type {
        IsolationType::Vbs => {
            hvcall()
                .accept_vtl2_pages(range, hvdef::hypercall::AcceptMemoryType::RAM)
                .expect("accepting vtl 2 memory must not fail");
        }
        IsolationType::Snp => {
            super::snp::set_page_acceptance(local_map.as_mut().unwrap(), range, true)
                .expect("accepting vtl 2 memory must not fail");
        }
        IsolationType::Tdx => {
            super::tdx::accept_pages(range).expect("accepting vtl2 memory must not fail")
        }
        _ => unreachable!(),
    }
}

/// Accepts VTL2 memory in the specified range that is currently marked as pending, i.e. not
/// yet assigned as exclusive and private.
fn accept_pending_vtl2_memory(
    shim_params: &ShimParams,
    local_map: &mut Option<LocalMap<'_>>,
    ram_buffer: &mut [u8],
    range: MemoryRange,
) {
    let isolation_type = shim_params.isolation_type;

    match isolation_type {
        IsolationType::Vbs => {
            hvcall()
                .accept_vtl2_pages(range, hvdef::hypercall::AcceptMemoryType::RAM)
                .expect("accepting vtl 2 memory must not fail");
        }
        IsolationType::Snp | IsolationType::Tdx => {
            let local_map = local_map.as_mut().unwrap();
            // Accepting pending memory for SNP is somewhat more complicated. The pending regions
            // are unencrypted pages. Accepting them would result in their contents being scrambled.
            // Instead their contents must be copied out to a private region, then copied back once
            // the pages have been accepted. Additionally, the access to the unencrypted pages must
            // happen with the C-bit cleared.
            let mut remaining = range;
            while !remaining.is_empty() {
                // Copy up to the next 2MB boundary.
                let range = MemoryRange::new(
                    remaining.start()
                        ..remaining.end().min(
                            (remaining.start() + X64_LARGE_PAGE_SIZE) & !(X64_LARGE_PAGE_SIZE - 1),
                        ),
                );
                remaining = MemoryRange::new(range.end()..remaining.end());

                let ram_buffer = &mut ram_buffer[..range.len() as usize];

                // Map the pages as shared and copy the necessary number to the buffer.
                {
                    let map_range = if isolation_type == IsolationType::Tdx {
                        // set vtom on the page number
                        MemoryRange::new(
                            range.start() | TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT
                                ..range.end() | TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT,
                        )
                    } else {
                        range
                    };

                    let mapping = local_map.map_pages(map_range, false);
                    ram_buffer.copy_from_slice(mapping.data);
                }

                // Change visibility on the pages for this iteration.
                match isolation_type {
                    IsolationType::Snp => {
                        super::snp::Ghcb::change_page_visibility(range, false);
                    }
                    IsolationType::Tdx => {
                        super::tdx::change_page_visibility(range, false);
                    }
                    _ => unreachable!(),
                }

                // accept the pages.
                match isolation_type {
                    IsolationType::Snp => {
                        super::snp::set_page_acceptance(local_map, range, true)
                            .expect("accepting vtl 2 memory must not fail");
                    }
                    IsolationType::Tdx => {
                        super::tdx::accept_pages(range)
                            .expect("accepting vtl 2 memory must not fail");
                    }
                    _ => unreachable!(),
                }

                // Copy the buffer back. Use the identity map now that the memory has been accepted.
                {
                    // SAFETY: Known memory region that was just accepted.
                    let mapping = unsafe {
                        core::slice::from_raw_parts_mut(
                            range.start() as *mut u8,
                            range.len() as usize,
                        )
                    };

                    mapping.copy_from_slice(ram_buffer);
                }
            }
        }
        _ => unreachable!(),
    }
}

// Verify the SHA384 hash of pages that were imported as unaccepted/shared. Compare against the
// desired hash that is passed in as a measured parameter. Failures result in a panic.
pub fn verify_imported_regions_hash(shim_params: &ShimParams) {
    // Non isolated VMs can undergo servicing, and thus the hash might no longer be valid,
    // as the memory regions can change during runtime.
    if let IsolationType::None = shim_params.isolation_type {
        return;
    }

    // If all imported pages are already accepted, there is no need to verify the hash.
    if shim_params
        .imported_regions()
        .all(|(_, already_accepted)| already_accepted)
    {
        return;
    }

    let mut hasher = Sha384::new();
    shim_params
        .imported_regions()
        .filter(|(_, already_accepted)| !already_accepted)
        .for_each(|(range, _)| {
            // SAFETY: The location and identity of the range is trusted as it is obtained from
            // measured shim parameters.
            let mapping = unsafe {
                core::slice::from_raw_parts(range.start() as *const u8, range.len() as usize)
            };
            hasher.update(mapping);
        });

    if hasher.finalize().as_slice() != shim_params.imported_regions_hash() {
        panic!("Imported regions hash mismatch");
    }
}
