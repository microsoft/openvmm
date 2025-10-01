// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parse partition info using the IGVM device tree parameter.

use super::PartitionInfo;
use super::shim_params::ShimParams;
use crate::boot_logger::log;
use crate::cmdline::BootCommandLineOptions;
use crate::host_params::COMMAND_LINE_SIZE;
use crate::host_params::MAX_CPU_COUNT;
use crate::host_params::MAX_ENTROPY_SIZE;
use crate::host_params::MAX_NUMA_NODES;
use crate::host_params::MAX_PARTITION_RAM_RANGES;
use crate::host_params::mmio::select_vtl2_mmio_range;
use crate::host_params::shim_params::IsolationType;
use crate::memory::AddressSpaceManager;
use crate::memory::AddressSpaceManagerBuilder;
use crate::memory::AllocationPolicy;
use crate::memory::AllocationType;
use crate::single_threaded::OffStackRef;
use crate::single_threaded::off_stack;
use arrayvec::ArrayVec;
use core::cmp::max;
use core::fmt::Write;
use host_fdt_parser::MemoryAllocationMode;
use host_fdt_parser::MemoryEntry;
use host_fdt_parser::ParsedDeviceTree;
use hvdef::HV_PAGE_SIZE;
use igvm_defs::MemoryMapEntryType;
use loader_defs::paravisor::CommandLinePolicy;
use memory_range::MemoryRange;
use memory_range::subtract_ranges;
use memory_range::walk_ranges;
use thiserror::Error;

mod bump_alloc;

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
    NotEnoughMmio,
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
        bump_alloc::ALLOCATOR.init(params.heap);
    }

    // TODO: test using heap, as no mesh decode yet.
    {
        use alloc::boxed::Box;
        bump_alloc::ALLOCATOR.enable_alloc();

        let box_int = Box::new(42);
        log!("box int {box_int}");
        drop(box_int);
        bump_alloc::ALLOCATOR.disable_alloc();
        bump_alloc::ALLOCATOR.log_stats();
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

        let mut dt_storage = off_stack!(ParsedDeviceTree<MAX_PARTITION_RAM_RANGES, MAX_CPU_COUNT, COMMAND_LINE_SIZE, MAX_ENTROPY_SIZE>, ParsedDeviceTree::new());

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

        // Depending on policy, write what the host specified in the chosen node.
        if can_trust_host && command_line.policy == CommandLinePolicy::APPEND_CHOSEN {
            // Parse in extra options from the host provided command line.
            options.parse(&parsed.command_line);
            write!(storage.cmdline, " {}", &parsed.command_line)
                .map_err(|_| DtError::CommandLineSize)?;
        }

        // TODO: Decide if isolated guests always use VTL2 allocation mode.

        match parsed.memory_allocation_mode {
            MemoryAllocationMode::Host => {
                storage.vtl2_ram.clear();
                storage
                    .vtl2_ram
                    .try_extend_from_slice(parse_host_vtl2_ram(params, &parsed.memory).as_ref())
                    .expect("vtl2 ram should only be 64 big");
                storage.memory_allocation_mode = MemoryAllocationMode::Host;
            }
            MemoryAllocationMode::Vtl2 {
                memory_size,
                mmio_size,
            } => {
                storage.vtl2_ram.clear();
                storage
                    .vtl2_ram
                    .try_extend_from_slice(
                        allocate_vtl2_ram(params, &parsed.memory, memory_size).as_ref(),
                    )
                    .expect("vtl2 ram should only be 64 big");
                storage.memory_allocation_mode = MemoryAllocationMode::Vtl2 {
                    memory_size,
                    mmio_size,
                };
            }
        }

        storage.vmbus_vtl2 = parsed.vmbus_vtl2.clone().ok_or(DtError::Vtl2Vmbus)?;
        storage.vmbus_vtl0 = parsed.vmbus_vtl0.clone().ok_or(DtError::Vtl0Vmbus)?;

        init_heap(params);

        // The host is responsible for allocating MMIO ranges for non-isolated
        // guests when it also provides the ram VTL2 should use.
        //
        // For isolated guests, or when VTL2 has been asked to carve out its own
        // memory, carve out a range from the VTL0 allotment.
        if params.isolation_type != IsolationType::None
            || matches!(
                parsed.memory_allocation_mode,
                MemoryAllocationMode::Vtl2 { .. }
            )
        {
            // Decide the amount of mmio VTL2 should allocate. Enforce a minimum
            // of 128 MB mmio for VTL2.
            const MINIMUM_MMIO_SIZE: u64 = 128 * (1 << 20);
            let mmio_size = max(
                match parsed.memory_allocation_mode {
                    MemoryAllocationMode::Vtl2 { mmio_size, .. } => mmio_size.unwrap_or(0),
                    _ => 0,
                },
                MINIMUM_MMIO_SIZE,
            );

            // Decide what mmio vtl2 should use.
            let mmio = &parsed.vmbus_vtl0.as_ref().ok_or(DtError::Vtl0Vmbus)?.mmio;
            let selected_vtl2_mmio = select_vtl2_mmio_range(mmio, mmio_size)?;

            // Update vtl0 mmio to exclude vtl2 mmio.
            let vtl0_mmio = subtract_ranges(
                storage.vmbus_vtl0.mmio.iter().cloned(),
                [selected_vtl2_mmio],
            )
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

            storage.vmbus_vtl2.mmio.clear();
            storage.vmbus_vtl2.mmio.push(selected_vtl2_mmio);
            storage.vmbus_vtl0.mmio = vtl0_mmio;
        }

        // The host provided device tree is marked as normal ram, as the
        // bootshim is responsible for constructing anything usermode needs from
        // it, and passing it via the device tree provided to the kernel.
        let reclaim_base = params.dt_start();
        let reclaim_end = params.dt_start() + params.dt_size();
        let vtl2_config_region_reclaim =
            MemoryRange::try_new(reclaim_base..reclaim_end).expect("range is valid");

        log!("reclaim device tree memory {reclaim_base:x}-{reclaim_end:x}");

        for entry in &parsed.memory {
            storage.partition_ram.push(*entry);
        }

        // Initialize the address space manager with fixed at build time ranges.
        let vtl2_config_region = MemoryRange::new(
            params.parameter_region_start
                ..(params.parameter_region_start + params.parameter_region_size),
        );

        let mut address_space_builder = AddressSpaceManagerBuilder::new(
            address_space,
            &storage.vtl2_ram,
            params.used,
            subtract_ranges([vtl2_config_region], [vtl2_config_region_reclaim]),
        )
        .with_log_buffer(params.log_buffer);

        if params.vtl2_reserved_region_size != 0 {
            address_space_builder = address_space_builder.with_reserved_range(MemoryRange::new(
                params.vtl2_reserved_region_start
                    ..(params.vtl2_reserved_region_start + params.vtl2_reserved_region_size),
            ));
        }

        if params.sidecar_size != 0 {
            address_space_builder = address_space_builder.with_sidecar_image(MemoryRange::new(
                params.sidecar_base..(params.sidecar_base + params.sidecar_size),
            ));
        }

        // Only specify pagetables as a reserved region on TDX, as they are used
        // for AP startup via the mailbox protocol. On other platforms, the
        // memory is free to be reclaimed.
        if params.isolation_type == IsolationType::Tdx {
            assert!(params.page_tables.is_some());
            address_space_builder = address_space_builder
                .with_page_tables(params.page_tables.expect("always present on tdx"));
        }

        address_space_builder
            .init()
            .expect("failed to initialize address space manager");

        // Decide if we will reserve memory for a VTL2 private pool. Parse this
        // from the final command line, or the host provided device tree value.
        let vtl2_gpa_pool_size = {
            let dt_page_count = parsed.device_dma_page_count;
            let cmdline_page_count = options.enable_vtl2_gpa_pool;
            max(dt_page_count.unwrap_or(0), cmdline_page_count.unwrap_or(0))
        };
        if vtl2_gpa_pool_size != 0 {
            // Reserve the specified number of pages for the pool. Use the used
            // ranges to figure out which VTL2 memory is free to allocate from.
            let pool_size_bytes = vtl2_gpa_pool_size * HV_PAGE_SIZE;

            match address_space.allocate(
                None,
                pool_size_bytes,
                AllocationType::GpaPool,
                AllocationPolicy::LowMemory,
            ) {
                Some(pool) => {
                    log!("allocated VTL2 pool at {:#x?}", pool.range);
                }
                None => {
                    panic!("failed to allocate VTL2 pool of size {pool_size_bytes:#x} bytes");
                }
            };
        }

        // If we can trust the host, use the provided alias map
        if can_trust_host {
            storage.vtl0_alias_map = parsed.vtl0_alias_map;
        }

        // Set remaining struct fields before returning.
        let Self {
            vtl2_ram: _,
            partition_ram: _,
            isolation,
            bsp_reg,
            cpus,
            vmbus_vtl0: _,
            vmbus_vtl2: _,
            cmdline: _,
            com3_serial_available: com3_serial,
            gic,
            pmu_gsiv,
            memory_allocation_mode: _,
            entropy,
            vtl0_alias_map: _,
            nvme_keepalive,
            boot_options,
        } = storage;

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
