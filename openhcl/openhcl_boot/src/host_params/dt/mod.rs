// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parse partition info using the IGVM device tree parameter.

extern crate alloc;

use super::PartitionInfo;
use super::shim_params::ShimParams;
use crate::boot_logger::log;
use crate::cmdline::BootCommandLineOptions;
use crate::cmdline::SidecarOptions;
use crate::host_params::COMMAND_LINE_SIZE;
use crate::host_params::MAX_CPU_COUNT;
use crate::host_params::MAX_ENTROPY_SIZE;
use crate::host_params::MAX_NUMA_NODES;
use crate::host_params::MAX_PARTITION_RAM_RANGES;
use crate::host_params::MAX_VTL2_RAM_RANGES;
use crate::host_params::dt::dma_hint::pick_private_pool_size;
use crate::host_params::mmio::select_vtl2_mmio_range;
use crate::host_params::shim_params::IsolationType;
use crate::memory::AddressSpaceManager;
use crate::memory::AddressSpaceManagerBuilder;
use crate::memory::AllocationPolicy;
use crate::memory::AllocationType;
use crate::single_threaded::OffStackRef;
use crate::single_threaded::off_stack;
use alloc::vec::Vec;
use arrayvec::ArrayVec;
use bump_alloc::ALLOCATOR;
use core::cmp::max;
use core::fmt::Write;
use host_fdt_parser::MemoryAllocationMode;
use host_fdt_parser::MemoryEntry;
use host_fdt_parser::ParsedDeviceTree;
use host_fdt_parser::VmbusInfo;
use hvdef::HV_PAGE_SIZE;
use igvm_defs::MemoryMapEntryType;
use loader_defs::paravisor::CommandLinePolicy;
use loader_defs::shim::MemoryVtlType;
use loader_defs::shim::PersistedStateHeader;
use memory_range::MemoryRange;
use memory_range::subtract_ranges;
use memory_range::walk_ranges;
use thiserror::Error;
use zerocopy::FromBytes;

mod bump_alloc;
mod dma_hint;

/// Errors when reading the host device tree.
#[derive(Debug, Error)]
pub enum DtError {
    /// Host did not provide a device tree.
    #[error("no device tree provided by host")]
    NoDeviceTree,
    /// Invalid device tree.
    #[error("host provided device tree is invalid")]
    DeviceTree(#[source] host_fdt_parser::Error<'static>),
    /// PartitionInfo's command line is too small to write the parsed legacy
    /// command line.
    #[error("commandline storage is too small to write the parsed command line")]
    CommandLineSize,
    /// Device tree did not contain a vmbus node for VTL2.
    #[error("device tree did not contain a vmbus node for VTL2")]
    Vtl2Vmbus,
    /// Device tree did not contain a vmbus node for VTL0.
    #[error("device tree did not contain a vmbus node for VTL0")]
    Vtl0Vmbus,
    /// Host provided high MMIO range is insufficient to cover VTL0 and VTL2.
    #[error("host provided high MMIO range is insufficient to cover VTL0 and VTL2")]
    NotEnoughVtl0Mmio,
    /// Host provided MMIO range is insufficient to cover VTL2.
    #[error("host provided MMIO range is insufficient to cover VTL2")]
    NotEnoughVtl2Mmio,
}

/// Allocate VTL2 ram from the partition's memory map.
fn allocate_vtl2_ram(
    params: &ShimParams,
    partition_memory_map: &[MemoryEntry],
    ram_size: Option<u64>,
) -> OffStackRef<'static, impl AsRef<[MemoryEntry]> + use<>> {
    // First, calculate how many numa nodes there are by looking at unique numa
    // nodes in the memory map.
    let mut numa_nodes = off_stack!(ArrayVec<u32, MAX_NUMA_NODES>, ArrayVec::new_const());

    for entry in partition_memory_map.iter() {
        match numa_nodes.binary_search(&entry.vnode) {
            Ok(_) => {}
            Err(index) => {
                numa_nodes.insert(index, entry.vnode);
            }
        }
    }

    let numa_node_count = numa_nodes.len();

    let vtl2_size = if let Some(ram_size) = ram_size {
        if ram_size < params.memory_size {
            panic!(
                "host provided vtl2 ram size {:x} is smaller than measured size {:x}",
                ram_size, params.memory_size
            );
        }
        max(ram_size, params.memory_size)
    } else {
        params.memory_size
    };

    // Next, calculate the amount of memory that needs to be allocated per numa
    // node.
    let ram_per_node = vtl2_size / numa_node_count as u64;

    // Seed the remaining allocation list with the memory required per node.
    let mut memory_per_node = off_stack!(ArrayVec<u64, MAX_NUMA_NODES>, ArrayVec::new_const());
    memory_per_node.extend((0..numa_node_count).map(|_| 0));
    for entry in partition_memory_map.iter() {
        memory_per_node[entry.vnode as usize] = ram_per_node;
    }

    // The range the IGVM file was loaded into is special - it is already
    // counted as "allocated". This may have been split across different numa
    // nodes. Walk the used range, add it to vtl2 ram, and subtract it from the
    // used ranges.
    let mut vtl2_ram = off_stack!(ArrayVec<MemoryEntry, MAX_NUMA_NODES>, ArrayVec::new_const());
    let mut free_memory_after_vtl2 = off_stack!(ArrayVec<MemoryEntry, 1024>, ArrayVec::new_const());
    let file_memory_range = MemoryRange::new(
        params.memory_start_address..(params.memory_start_address + params.memory_size),
    );

    for (range, result) in walk_ranges(
        [(file_memory_range, ())],
        partition_memory_map.iter().map(|e| (e.range, e)),
    ) {
        match result {
            memory_range::RangeWalkResult::Right(entry) => {
                // Add this entry to the free list.
                free_memory_after_vtl2.push(MemoryEntry {
                    range,
                    mem_type: entry.mem_type,
                    vnode: entry.vnode,
                });
            }
            memory_range::RangeWalkResult::Both(_, entry) => {
                // Add this entry to the vtl2 ram list.
                vtl2_ram.push(MemoryEntry {
                    range,
                    mem_type: entry.mem_type,
                    vnode: entry.vnode,
                });
            }
            memory_range::RangeWalkResult::Left(_) => {
                panic!("used file range {range:#x?} is not reported as ram by host memmap")
            }
            // Ranges in neither are ignored.
            memory_range::RangeWalkResult::Neither => {}
        }
    }

    // Now remove ranges from the free list that were part of the initial launch
    // context.
    let mut free_memory = off_stack!(ArrayVec<MemoryEntry, 1024>, ArrayVec::new_const());
    for (range, result) in walk_ranges(
        params
            .imported_regions()
            .filter_map(|(range, _preaccepted)| {
                if !file_memory_range.contains(&range) {
                     // There should be no overlap - either the preaccepted range
                    // is exclusively covered by the preaccpted VTL2 range or it
                    // is not.
                    assert!(!file_memory_range.overlaps(&range), "imported range {range:#x?} overlaps vtl2 range and is not fully contained within vtl2 range");
                    Some((range, ()))
                } else {
                    None
                }
            }),
        free_memory_after_vtl2.iter().map(|e| (e.range, e)),
    ) {
        match result {
            memory_range::RangeWalkResult::Right(entry) => {
                free_memory.push(MemoryEntry {
                    range,
                    mem_type: entry.mem_type,
                    vnode: entry.vnode,
                });
            }
            memory_range::RangeWalkResult::Left(_) => {
                // On TDX, the reset vector page is not reported as ram by the
                // host, but is preaccepted. Ignore it.
                #[cfg(target_arch = "x86_64")]
                if params.isolation_type == IsolationType::Tdx && range.start_4k_gpn() == 0xFFFFF && range.len() == 0x1000 {
                    continue;
                }

                panic!("launch context range {range:#x?} is not reported as ram by host memmap")
            }
            memory_range::RangeWalkResult::Both(_, _) => {
                // Range was part of the preaccepted import, is not free to
                // allocate additional VTL2 ram from.
            }
            // Ranges in neither are ignored.
            memory_range::RangeWalkResult::Neither => {}
        }
    }

    // Subtract the used ranges from vtl2_ram
    for entry in vtl2_ram.iter() {
        let mem_req = &mut memory_per_node[entry.vnode as usize];

        if entry.range.len() > *mem_req {
            // TODO: Today if a used range is larger than the mem required, we
            // just subtract that numa range to zero. Should we instead subtract
            // from other numa nodes equally for over allocation?
            log!(
                "entry {entry:?} is larger than required {mem_req} for vnode {}",
                entry.vnode
            );
            *mem_req = 0;
        } else {
            *mem_req -= entry.range.len();
        }
    }

    // Allocate remaining memory per node required.
    for (node, required_mem) in memory_per_node.iter().enumerate() {
        let mut required_mem = *required_mem;
        if required_mem == 0 {
            continue;
        }

        // Start allocation from the top of the free list, which is high memory
        // in reverse order.
        for entry in free_memory.iter_mut().rev() {
            if entry.vnode == node as u32 && !entry.range.is_empty() {
                assert!(required_mem != 0);
                let bytes_to_allocate = core::cmp::min(entry.range.len(), required_mem);

                // Allocate top down from the range.
                let offset = entry.range.len() - bytes_to_allocate;
                let (remaining, alloc) = MemoryRange::split_at_offset(&entry.range, offset);

                entry.range = remaining;
                vtl2_ram.push(MemoryEntry {
                    range: alloc,
                    mem_type: entry.mem_type,
                    vnode: node as u32,
                });

                required_mem -= bytes_to_allocate;

                // Stop allocating if we're done allocating.
                if required_mem == 0 {
                    break;
                }
            }
        }

        if required_mem != 0 {
            // TODO: Handle fallback allocations on other numa nodes when a node
            // is exhausted.
            panic!(
                "failed to allocate {required_mem:#x} for vnode {node:#x}, no memory remaining for vnode"
            );
        }
    }

    // Sort VTL2 ram as we may have allocated from different places.
    vtl2_ram.sort_unstable_by_key(|e| e.range.start());

    vtl2_ram
}

/// Parse VTL2 ram from host provided ranges.
fn parse_host_vtl2_ram(
    params: &ShimParams,
    memory: &[MemoryEntry],
) -> OffStackRef<'static, impl AsRef<[MemoryEntry]> + use<>> {
    // If no VTL2 protectable ram was provided by the host, use the build time
    // value encoded in ShimParams.
    let mut vtl2_ram = off_stack!(ArrayVec<MemoryEntry, MAX_NUMA_NODES>, ArrayVec::new_const());
    if params.isolation_type.is_hardware_isolated() {
        // Hardware isolated VMs use the size hint by the host, but use the base
        // address encoded in the file.
        let vtl2_size = memory.iter().fold(0, |acc, entry| {
            if entry.mem_type == MemoryMapEntryType::VTL2_PROTECTABLE {
                acc + entry.range.len()
            } else {
                acc
            }
        });

        log!(
            "host provided vtl2 ram size is {:x}, measured size is {:x}",
            vtl2_size,
            params.memory_size
        );

        let vtl2_size = max(vtl2_size, params.memory_size);
        vtl2_ram.push(MemoryEntry {
            range: MemoryRange::new(
                params.memory_start_address..(params.memory_start_address + vtl2_size),
            ),
            mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
            vnode: 0,
        });
    } else {
        for &entry in memory
            .iter()
            .filter(|entry| entry.mem_type == MemoryMapEntryType::VTL2_PROTECTABLE)
        {
            vtl2_ram.push(entry);
        }
    }

    if vtl2_ram.is_empty() {
        log!("using measured vtl2 ram");
        vtl2_ram.push(MemoryEntry {
            range: MemoryRange::try_new(
                params.memory_start_address..(params.memory_start_address + params.memory_size),
            )
            .expect("range is valid"),
            mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
            vnode: 0,
        });
    }

    vtl2_ram
}

fn init_heap(params: &ShimParams) {
    // Initialize the temporary heap.
    //
    // This is only to be enabled for mesh decode.
    //
    // SAFETY: The heap range is reserved at file build time, and is
    // guaranteed to be unused by anything else.
    unsafe {
        ALLOCATOR.init(params.heap);
    }
}

type ParsedDt =
    ParsedDeviceTree<MAX_PARTITION_RAM_RANGES, MAX_CPU_COUNT, COMMAND_LINE_SIZE, MAX_ENTROPY_SIZE>;

/// Add common ranges to [`AddressSpaceManagerBuilder`] regardless if creating
/// topology from the host or from saved state.
fn add_common_ranges<'a, I: Iterator<Item = MemoryRange>>(
    params: &ShimParams,
    mut builder: AddressSpaceManagerBuilder<'a, I>,
) -> AddressSpaceManagerBuilder<'a, I> {
    // Add the log buffer which is always present.
    builder = builder.with_log_buffer(params.log_buffer);

    if params.vtl2_reserved_region_size != 0 {
        builder = builder.with_reserved_range(MemoryRange::new(
            params.vtl2_reserved_region_start
                ..(params.vtl2_reserved_region_start + params.vtl2_reserved_region_size),
        ));
    }

    if params.sidecar_size != 0 {
        builder = builder.with_sidecar_image(MemoryRange::new(
            params.sidecar_base..(params.sidecar_base + params.sidecar_size),
        ));
    }

    builder
}

#[derive(Debug, PartialEq, Eq)]
struct PartitionTopology {
    vtl2_ram: &'static [MemoryEntry],
    vtl0_mmio: ArrayVec<MemoryRange, 2>,
    vtl2_mmio: ArrayVec<MemoryRange, 2>,
    memory_allocation_mode: MemoryAllocationMode,
}

/// State derived while constructing the partition topology
/// from persisted state.
#[derive(Debug, PartialEq, Eq)]
struct PersistedPartitionTopology {
    topology: PartitionTopology,
    cpus_with_mapped_interrupts_no_io: Vec<u32>,
    cpus_with_outstanding_io: Vec<u32>,
}

// Calculate the default mmio size for VTL2 when not specified by the host.
//
// This is half of the high mmio gap size, rounded down, with a minimum of 128
// MB and a maximum of 1 GB.
fn calculate_default_mmio_size(parsed: &ParsedDt) -> Result<u64, DtError> {
    const MINIMUM_MMIO_SIZE: u64 = 128 * (1 << 20);
    const MAXIMUM_MMIO_SIZE: u64 = 1 << 30;
    let half_high_gap = parsed.vmbus_vtl0.as_ref().ok_or(DtError::Vtl0Vmbus)?.mmio[1].len() / 2;
    Ok(half_high_gap.clamp(MINIMUM_MMIO_SIZE, MAXIMUM_MMIO_SIZE))
}

/// Read topology from the host provided device tree.
fn topology_from_host_dt(
    params: &ShimParams,
    parsed: &ParsedDt,
    options: &BootCommandLineOptions,
    address_space: &mut AddressSpaceManager,
) -> Result<PartitionTopology, DtError> {
    log!("reading topology from host device tree");

    let mut vtl2_ram =
        off_stack!(ArrayVec<MemoryEntry, MAX_VTL2_RAM_RANGES>, ArrayVec::new_const());

    // TODO: Decide if isolated guests always use VTL2 allocation mode.

    let memory_allocation_mode = parsed.memory_allocation_mode;
    match memory_allocation_mode {
        MemoryAllocationMode::Host => {
            vtl2_ram
                .try_extend_from_slice(parse_host_vtl2_ram(params, &parsed.memory).as_ref())
                .expect("vtl2 ram should only be 64 big");
        }
        MemoryAllocationMode::Vtl2 {
            memory_size,
            mmio_size: _,
        } => {
            vtl2_ram
                .try_extend_from_slice(
                    allocate_vtl2_ram(params, &parsed.memory, memory_size).as_ref(),
                )
                .expect("vtl2 ram should only be 64 big");
        }
    }

    // The host is responsible for allocating MMIO ranges for non-isolated
    // guests when it also provides the ram VTL2 should use.
    //
    // For isolated guests, or when VTL2 has been asked to carve out its own
    // memory, first check if the host provided a VTL2 mmio range. If so, the
    // mmio range must be large enough. Otherwise, choose to carve out a range
    // from the VTL0 allotment.
    let (vtl0_mmio, vtl2_mmio) = if params.isolation_type != IsolationType::None
        || matches!(
            parsed.memory_allocation_mode,
            MemoryAllocationMode::Vtl2 { .. }
        ) {
        // Decide the amount of mmio VTL2 should allocate, which is different
        // depending on the heuristic used.
        //
        // On a newer host where a vtl2 mmio range is provided inside the
        // vmbus_vtl2 device tree node, use the size provided by the host inside
        // the openhcl node for memory allocation mode.
        //
        // If the host did not provide a vtl2 mmio range, then use the maximum
        // of the host provided value inside the openhcl node and the calculated
        // default.
        let host_provided_size = match parsed.memory_allocation_mode {
            MemoryAllocationMode::Vtl2 { mmio_size, .. } => mmio_size.unwrap_or(0),
            _ => 0,
        };
        let vmbus_vtl2 = parsed.vmbus_vtl2.as_ref().ok_or(DtError::Vtl2Vmbus)?;
        let vmbus_vtl2_mmio_size = vmbus_vtl2.mmio.iter().map(|r| r.len()).sum::<u64>();
        let mmio_size = if vmbus_vtl2_mmio_size != 0 {
            host_provided_size
        } else {
            max(host_provided_size, calculate_default_mmio_size(parsed)?)
        };

        log!("allocating vtl2 mmio size {mmio_size:#x} bytes");
        log!("host provided vtl2 mmio ranges are {vmbus_vtl2_mmio_size:#x} bytes");

        let vmbus_vtl0 = parsed.vmbus_vtl0.as_ref().ok_or(DtError::Vtl0Vmbus)?;
        if vmbus_vtl2_mmio_size != 0 {
            // Verify the host provided mmio is large enough.
            if vmbus_vtl2_mmio_size < mmio_size {
                return Err(DtError::NotEnoughVtl2Mmio);
            }

            log!("using host provided vtl2 mmio: {:x?}", vmbus_vtl2.mmio);
            (vmbus_vtl0.mmio.clone(), vmbus_vtl2.mmio.clone())
        } else {
            // Allocate vtl2 mmio from vtl0 mmio.
            log!("no vtl2 mmio provided by host, allocating from vtl0 mmio");
            let selected_vtl2_mmio = select_vtl2_mmio_range(&vmbus_vtl0.mmio, mmio_size)?;

            // Update vtl0 mmio to exclude vtl2 mmio.
            let vtl0_mmio = subtract_ranges(vmbus_vtl0.mmio.iter().cloned(), [selected_vtl2_mmio])
                .collect::<ArrayVec<MemoryRange, 2>>();
            let vtl2_mmio = [selected_vtl2_mmio]
                .into_iter()
                .collect::<ArrayVec<MemoryRange, 2>>();

            // TODO: For now, if we have only a single vtl0_mmio range left,
            // panic. In the future decide if we want to report this as a start
            // failure in usermode, change allocation strategy, or something
            // else.
            assert_eq!(
                vtl0_mmio.len(),
                2,
                "vtl0 mmio ranges are not 2 {:#x?}",
                vtl0_mmio
            );

            log!("vtl0 mmio: {vtl0_mmio:x?}, vtl2 mmio: {vtl2_mmio:x?}");

            (vtl0_mmio, vtl2_mmio)
        }
    } else {
        (
            parsed
                .vmbus_vtl0
                .as_ref()
                .ok_or(DtError::Vtl0Vmbus)?
                .mmio
                .clone(),
            parsed
                .vmbus_vtl2
                .as_ref()
                .ok_or(DtError::Vtl2Vmbus)?
                .mmio
                .clone(),
        )
    };

    // The host provided device tree is marked as normal ram, as the
    // bootshim is responsible for constructing anything usermode needs from
    // it, and passing it via the device tree provided to the kernel.
    let reclaim_base = params.dt_start();
    let reclaim_end = params.dt_start() + params.dt_size();
    let vtl2_config_region_reclaim =
        MemoryRange::try_new(reclaim_base..reclaim_end).expect("range is valid");

    log!("reclaim device tree memory {reclaim_base:x}-{reclaim_end:x}");

    // Initialize the address space manager with fixed at build time ranges.
    let vtl2_config_region = MemoryRange::new(
        params.parameter_region_start
            ..(params.parameter_region_start + params.parameter_region_size),
    );

    // NOTE: Size the region as 20 pages. This should be plenty enough for the
    // worst case encoded size (about 50 bytes worst case per memory entry, with
    // the max number of ram ranges), and is small enough that we can reserve it
    // on all sizes. Revisit this calculation if we persist more state in the
    // future.
    const PERSISTED_REGION_SIZE: u64 = 20 * 4096;
    let (persisted_state_region, remainder) = params
        .persisted_state
        .split_at_offset(PERSISTED_REGION_SIZE);
    log!("persisted state region sized to {persisted_state_region:#x?}, remainder {remainder:#x?}");

    let mut address_space_builder = AddressSpaceManagerBuilder::new(
        address_space,
        &vtl2_ram,
        params.used,
        persisted_state_region,
        subtract_ranges([vtl2_config_region], [vtl2_config_region_reclaim]),
    );

    address_space_builder = add_common_ranges(params, address_space_builder);

    address_space_builder
        .init()
        .expect("failed to initialize address space manager");

    if params.isolation_type == IsolationType::None {
        let enable_vtl2_gpa_pool = options.enable_vtl2_gpa_pool;
        let device_dma_page_count = parsed.device_dma_page_count;
        let vp_count = parsed.cpu_count();
        let mem_size = vtl2_ram.iter().map(|e| e.range.len()).sum();
        if let Some(vtl2_gpa_pool_size) = pick_private_pool_size(
            enable_vtl2_gpa_pool,
            device_dma_page_count,
            vp_count,
            mem_size,
        ) {
            // Reserve the specified number of pages for the pool. Use the used
            // ranges to figure out which VTL2 memory is free to allocate from.
            let pool_size_bytes = vtl2_gpa_pool_size * HV_PAGE_SIZE;

            // NOTE: For now, allocate all the private pool on NUMA node 0 to
            // match previous behavior. Allocate from high memory downward to
            // avoid overlapping any used ranges in low memory when openhcl's
            // usage gets bigger, as otherwise the used_range by the bootshim
            // could overlap the pool range chosen, when servicing to a new
            // image.
            match address_space.allocate(
                Some(0),
                pool_size_bytes,
                AllocationType::GpaPool,
                AllocationPolicy::HighMemory,
            ) {
                Some(pool) => {
                    log!("allocated VTL2 pool at {:#x?}", pool.range);
                }
                None => {
                    panic!(
                        "failed to allocate VTL2 pool of size {pool_size_bytes:#x} bytes (enable_vtl2_gpa_pool={enable_vtl2_gpa_pool:?}, device_dma_page_count={device_dma_page_count:#x?}, vp_count={vp_count}, mem_size={mem_size:#x})"
                    );
                }
            };
        }
    }

    Ok(PartitionTopology {
        vtl2_ram: OffStackRef::<'_, ArrayVec<MemoryEntry, MAX_VTL2_RAM_RANGES>>::leak(vtl2_ram),
        vtl0_mmio,
        vtl2_mmio,
        memory_allocation_mode,
    })
}

/// Read topology from the persisted state region and protobuf payload.
fn topology_from_persisted_state(
    header: PersistedStateHeader,
    params: &ShimParams,
    parsed: &ParsedDt,
    address_space: &mut AddressSpaceManager,
) -> Result<PersistedPartitionTopology, DtError> {
    log!("reading topology from persisted state");

    // Verify the header describes a protobuf region within the bootshim
    // persisted region. We expect it to live there as today we rely on the
    // build time generated pagetable to identity map the protobuf region.
    let protobuf_region =
        MemoryRange::new(header.protobuf_base..(header.protobuf_base + header.protobuf_region_len));
    assert!(
        params.persisted_state.contains(&protobuf_region),
        "protobuf region {protobuf_region:#x?} is not contained within the persisted state region {:#x?}",
        params.persisted_state
    );

    // Verify protobuf payload len is smaller than region.
    assert!(
        header.protobuf_payload_len <= header.protobuf_region_len,
        "protobuf payload len {} is larger than region len {}",
        header.protobuf_payload_len,
        header.protobuf_region_len
    );

    // SAFETY: The region lies within the persisted state region, which is
    // identity mapped via the build time generated pagetable.
    let protobuf_raw = unsafe {
        core::slice::from_raw_parts(
            header.protobuf_base as *const u8,
            header.protobuf_payload_len as usize,
        )
    };

    let parsed_protobuf: loader_defs::shim::save_restore::SavedState =
        bump_alloc::with_global_alloc(|| {
            log!("decoding protobuf of size {}", protobuf_raw.len());
            mesh_protobuf::decode(protobuf_raw).expect("failed to decode protobuf")
        });

    let loader_defs::shim::save_restore::SavedState {
        partition_memory,
        partition_mmio,
        cpus_with_mapped_interrupts_no_io,
        cpus_with_outstanding_io,
    } = parsed_protobuf;

    // FUTURE: should memory allocation mode should persist in saved state and
    // verify the host did not change it?
    let memory_allocation_mode = parsed.memory_allocation_mode;

    let mut vtl2_ram =
        off_stack!(ArrayVec<MemoryEntry, MAX_VTL2_RAM_RANGES>, ArrayVec::new_const());

    // Determine which ranges are memory ranges used by VTL2.
    let previous_vtl2_ram = partition_memory.iter().filter_map(|entry| {
        if entry.vtl_type.ram() && entry.vtl_type.vtl2() {
            Some(MemoryEntry {
                range: entry.range,
                mem_type: entry.igvm_type.clone().into(),
                vnode: entry.vnode,
            })
        } else {
            None
        }
    });

    // Merge adjacent ranges as saved state reports the final usage of ram which
    // includes reserved in separate ranges. Here we want the whole underlying
    // ram ranges, merged with adjacent types if they share the same igvm types.
    let previous_vtl2_ram = memory_range::merge_adjacent_ranges(
        previous_vtl2_ram.map(|entry| (entry.range, (entry.mem_type, entry.vnode))),
    );

    vtl2_ram.extend(
        previous_vtl2_ram.map(|(range, (mem_type, vnode))| MemoryEntry {
            range,
            mem_type,
            vnode,
        }),
    );

    // If the host was responsible for allocating VTL2 ram, verify the ram
    // parsed from the previous instance matches.
    //
    // FUTURE: When VTL2 itself did allocation, we should verify that all ranges
    // are still within the provided memory map.
    if matches!(memory_allocation_mode, MemoryAllocationMode::Host) {
        let host_vtl2_ram = parse_host_vtl2_ram(params, &parsed.memory);
        assert_eq!(
            vtl2_ram.as_slice(),
            host_vtl2_ram.as_ref(),
            "vtl2 ram from persisted state does not match host provided ram"
        );
    }

    // Merge the persisted state header and protobuf region, and report that as
    // the persisted region.
    //
    // NOTE: We could choose to resize the persisted region at this point, which
    // we would need to do if we expect the saved state to grow larger.
    let persisted_header = partition_memory
        .iter()
        .find(|entry| entry.vtl_type == MemoryVtlType::VTL2_PERSISTED_STATE_HEADER)
        .expect("persisted state header missing");
    let persisted_protobuf = partition_memory
        .iter()
        .find(|entry| entry.vtl_type == MemoryVtlType::VTL2_PERSISTED_STATE_PROTOBUF)
        .expect("persisted state protobuf region missing");
    assert_eq!(persisted_header.range.end(), protobuf_region.start());
    let persisted_state_region =
        MemoryRange::new(persisted_header.range.start()..persisted_protobuf.range.end());

    // The host provided device tree is marked as normal ram, as the
    // bootshim is responsible for constructing anything usermode needs from
    // it, and passing it via the device tree provided to the kernel.
    let reclaim_base = params.dt_start();
    let reclaim_end = params.dt_start() + params.dt_size();
    let vtl2_config_region_reclaim =
        MemoryRange::try_new(reclaim_base..reclaim_end).expect("range is valid");

    log!("reclaim device tree memory {reclaim_base:x}-{reclaim_end:x}");

    let vtl2_config_region = MemoryRange::new(
        params.parameter_region_start
            ..(params.parameter_region_start + params.parameter_region_size),
    );

    let mut address_space_builder = AddressSpaceManagerBuilder::new(
        address_space,
        &vtl2_ram,
        params.used,
        persisted_state_region,
        subtract_ranges([vtl2_config_region], [vtl2_config_region_reclaim]),
    );

    // NOTE: The only other region we take from the previous instance is any
    // allocated vtl2 pool. Today, we do not allocate a new/larger pool if the
    // command line arguments or host device tree changed, as that's not
    // something we expect to happen in practice.
    let mut pool_ranges = partition_memory.iter().filter_map(|entry| {
        if entry.vtl_type == MemoryVtlType::VTL2_GPA_POOL {
            Some(entry.range)
        } else {
            None
        }
    });
    let pool_range = pool_ranges.next();
    assert!(
        pool_ranges.next().is_none(),
        "previous instance had multiple pool ranges"
    );

    if let Some(pool_range) = pool_range {
        address_space_builder = address_space_builder.with_pool_range(pool_range);
    }

    // As described above, other ranges come from this current boot.
    address_space_builder = add_common_ranges(params, address_space_builder);

    address_space_builder
        .init()
        .expect("failed to initialize address space manager");

    // Read previous mmio for VTL0 and VTL2.
    let vtl0_mmio = partition_mmio
        .iter()
        .filter_map(|entry| {
            if entry.vtl_type == MemoryVtlType::VTL0_MMIO {
                Some(entry.range)
            } else {
                None
            }
        })
        .collect::<ArrayVec<MemoryRange, 2>>();
    let vtl2_mmio = partition_mmio
        .iter()
        .filter_map(|entry| {
            if entry.vtl_type == MemoryVtlType::VTL2_MMIO {
                Some(entry.range)
            } else {
                None
            }
        })
        .collect::<ArrayVec<MemoryRange, 2>>();

    Ok(PersistedPartitionTopology {
        topology: PartitionTopology {
            vtl2_ram: OffStackRef::<'_, ArrayVec<MemoryEntry, MAX_VTL2_RAM_RANGES>>::leak(vtl2_ram),
            vtl0_mmio,
            vtl2_mmio,
            memory_allocation_mode,
        },
        cpus_with_mapped_interrupts_no_io,
        cpus_with_outstanding_io,
    })
}

/// Read the persisted header from the start of the persisted state region
/// described at file build time. If the magic value is not set, `None` is
/// returned.
fn read_persisted_region_header(params: &ShimParams) -> Option<PersistedStateHeader> {
    // TODO CVM: On an isolated guest, these pages may not be accepted. We need
    // to rethink how this will work in order to handle this correctly, as on a
    // first boot we'd need to accept them early, but subsequent boots should
    // not accept any pages.
    //
    // This may require some value passed in via a register or something early
    // that indicates this is a servicing boot, which we could set if OpenHCL
    // itself launches the next instance.
    if params.isolation_type != IsolationType::None {
        return None;
    }

    // SAFETY: The header lies at the start of the shim described persisted state
    // region. This range is guaranteed to be identity mapped at file build
    // time.
    let buf = unsafe {
        core::slice::from_raw_parts(
            params.persisted_state.start() as *const u8,
            size_of::<PersistedStateHeader>(),
        )
    };

    let header = PersistedStateHeader::read_from_bytes(buf)
        .expect("region is page aligned and the correct size");

    if header.magic == PersistedStateHeader::MAGIC {
        Some(header)
    } else {
        None
    }
}

impl PartitionInfo {
    // Read the IGVM provided DT for the vtl2 partition info.
    pub fn read_from_dt<'a>(
        params: &'a ShimParams,
        storage: &'a mut Self,
        address_space: &'_ mut AddressSpaceManager,
        mut options: BootCommandLineOptions,
        can_trust_host: bool,
    ) -> Result<&'a mut Self, DtError> {
        let dt = params.device_tree();

        if dt[0] == 0 {
            log!("host did not provide a device tree");
            return Err(DtError::NoDeviceTree);
        }

        let mut dt_storage = off_stack!(ParsedDt, ParsedDeviceTree::new());

        let parsed = ParsedDeviceTree::parse(dt, &mut *dt_storage).map_err(DtError::DeviceTree)?;

        let command_line = params.command_line();

        // Always write the measured command line.
        write!(
            storage.cmdline,
            "{}",
            command_line
                .command_line()
                .expect("measured command line should be valid")
        )
        .map_err(|_| DtError::CommandLineSize)?;

        match command_line.policy {
            CommandLinePolicy::STATIC => {
                // Nothing to do, we already wrote the measured command line.
            }
            CommandLinePolicy::APPEND_CHOSEN if can_trust_host => {
                // Check the host-provided command line for options for ourself,
                // and pass it along to the kernel.
                options.parse(&parsed.command_line);
                write!(storage.cmdline, " {}", &parsed.command_line)
                    .map_err(|_| DtError::CommandLineSize)?;
            }
            CommandLinePolicy::APPEND_CHOSEN if !can_trust_host => {
                // Nothing to do, we ignore the host provided command line.
            }
            _ => unreachable!(),
        }

        init_heap(params);

        let persisted_state_header = read_persisted_region_header(params);
        let (topology, has_devices_that_should_disable_sidecar) =
            if let Some(header) = persisted_state_header {
                log!("found persisted state header");
                let persisted_topology =
                    topology_from_persisted_state(header, params, parsed, address_space)?;

                (
                    persisted_topology.topology,
                    !(persisted_topology
                        .cpus_with_mapped_interrupts_no_io
                        .is_empty()
                        && persisted_topology.cpus_with_outstanding_io.is_empty()),
                )
            } else {
                (
                    topology_from_host_dt(params, parsed, &options, address_space)?,
                    false,
                )
            };

        let Self {
            vtl2_ram,
            partition_ram,
            isolation,
            bsp_reg,
            cpus,
            vmbus_vtl0,
            vmbus_vtl2,
            cmdline: _,
            com3_serial_available: com3_serial,
            gic,
            pmu_gsiv,
            memory_allocation_mode,
            entropy,
            vtl0_alias_map,
            nvme_keepalive,
            boot_options,
        } = storage;

        if let (SidecarOptions::Enabled { cpu_threshold, .. }, true) = (
            &boot_options.sidecar,
            has_devices_that_should_disable_sidecar,
        ) {
            if cpu_threshold.is_none()
                || cpu_threshold
                    .and_then(|threshold| threshold.try_into().ok())
                    .is_some_and(|threshold| parsed.cpu_count() < threshold)
            {
                // If we are in the restore path, disable sidecar for small VMs, as the amortization
                // benefits don't apply when devices are kept alive; the CPUs need to be powered on anyway
                // to check for interrupts.
                log!("disabling sidecar, as we are restoring from persisted state");
                boot_options.sidecar = SidecarOptions::DisabledServicing;
                options.sidecar = SidecarOptions::DisabledServicing;
            }
        }

        // Set ram and memory alloction mode.
        vtl2_ram.clear();
        vtl2_ram.extend(topology.vtl2_ram.iter().copied());
        partition_ram.clear();
        partition_ram.extend(parsed.memory.iter().copied());
        *memory_allocation_mode = topology.memory_allocation_mode;

        // Set vmbus fields. The connection ID comes from the host, but mmio
        // comes from topology.
        *vmbus_vtl0 = VmbusInfo {
            connection_id: parsed
                .vmbus_vtl0
                .as_ref()
                .ok_or(DtError::Vtl0Vmbus)?
                .connection_id,
            mmio: topology.vtl0_mmio,
        };
        *vmbus_vtl2 = VmbusInfo {
            connection_id: parsed
                .vmbus_vtl2
                .as_ref()
                .ok_or(DtError::Vtl2Vmbus)?
                .connection_id,
            mmio: topology.vtl2_mmio,
        };

        // If we can trust the host, use the provided alias map
        if can_trust_host {
            *vtl0_alias_map = parsed.vtl0_alias_map;
        }

        *isolation = params.isolation_type;

        *bsp_reg = parsed.boot_cpuid_phys;
        cpus.extend(parsed.cpus.iter().copied());
        *com3_serial = parsed.com3_serial;
        *gic = parsed.gic.clone();
        *pmu_gsiv = parsed.pmu_gsiv;
        *entropy = parsed.entropy.clone();
        *nvme_keepalive = parsed.nvme_keepalive;
        *boot_options = options;

        Ok(storage)
    }
}
