// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::boot_logger::log;
use crate::host_params::shim_params::IsolationType;
use crate::host_params::shim_params::ShimParams;
use crate::host_params::PartitionInfo;
use crate::host_params::MAX_CPU_COUNT;
use crate::host_params::MAX_NUMA_NODES;
use crate::single_threaded::off_stack;
use arrayvec::ArrayVec;
use memory_range::MemoryRange;
use sidecar_defs::SidecarNodeOutput;
use sidecar_defs::SidecarNodeParams;
use sidecar_defs::SidecarOutput;
use sidecar_defs::SidecarParams;

/// The maximum side of a sidecar node. This is tuned to ensure that there are
/// enough Linux CPUs to manage all the sidecar VPs.
const MAX_SIDECAR_NODE_SIZE: usize = 32;

// Assert that there are enough sidecar nodes for the maximum number of CPUs, if
// all NUMA nodes but one have one processor.
const _: () = assert!(
    sidecar_defs::MAX_NODES >= (MAX_NUMA_NODES - 1) + MAX_CPU_COUNT.div_ceil(MAX_SIDECAR_NODE_SIZE)
);

pub struct SidecarConfig<'a> {
    pub image: MemoryRange,
    pub node_params: &'a [SidecarNodeParams],
    pub nodes: &'a [SidecarNodeOutput],
    pub start_reftime: u64,
    pub end_reftime: u64,
}

impl SidecarConfig<'_> {
    /// Returns an object to be appended to the Linux kernel command line to
    /// configure it properly for sidecar.
    pub fn kernel_command_line(&self) -> SidecarKernelCommandLine<'_> {
        SidecarKernelCommandLine(self)
    }
}

pub struct SidecarKernelCommandLine<'a>(&'a SidecarConfig<'a>);

impl core::fmt::Display for SidecarKernelCommandLine<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Add something like boot_cpus=0,4,8,12 to the command line so that
        // Linux boots with the base VP of each sidecar node. Other CPUs will
        // be brought up by the sidecar kernel.
        f.write_str("boot_cpus=")?;
        let mut comma = "";
        for node in self.0.node_params {
            write!(f, "{}{}", comma, node.base_vp)?;
            comma = ",";
        }
        Ok(())
    }
}

pub fn start_sidecar<'a>(
    p: &ShimParams,
    partition_info: &PartitionInfo,
    sidecar_params: &'a mut SidecarParams,
    sidecar_output: &'a mut SidecarOutput,
) -> Option<SidecarConfig<'a>> {
    if !cfg!(target_arch = "x86_64")
        || p.isolation_type != IsolationType::None
        || p.sidecar_size == 0
    {
        return None;
    }
    let image = MemoryRange::new(p.sidecar_base..p.sidecar_base + p.sidecar_size);

    // Ensure the host didn't provide an out-of-bounds NUMA node.
    let max_vnode = partition_info
        .cpus
        .iter()
        .map(|cpu| cpu.vnode)
        .chain(partition_info.vtl2_ram.iter().map(|e| e.vnode))
        .max()
        .unwrap();

    if max_vnode >= MAX_NUMA_NODES as u32 {
        log!("sidecar: NUMA node {max_vnode} too large");
        return None;
    }

    // Compute a free list of VTL2 memory per NUMA node.
    let mut free_memory = off_stack!(ArrayVec<MemoryRange, MAX_NUMA_NODES>, ArrayVec::new_const());
    free_memory.extend((0..max_vnode + 1).map(|_| MemoryRange::EMPTY));
    for (range, r) in memory_range::walk_ranges(
        partition_info.vtl2_ram.iter().map(|e| (e.range, e.vnode)),
        partition_info
            .vtl2_used_ranges
            .iter()
            .cloned()
            .map(|range| (range, ())),
    ) {
        if let memory_range::RangeWalkResult::Left(vnode) = r {
            let free = &mut free_memory[vnode as usize];
            if range.len() > free.len() {
                *free = range;
            }
        }
    }

    // Split the CPUs by NUMA node, and then into chunks of no more than
    // MAX_SIDECAR_NODE_SIZE processors.
    let cpus_by_node = || {
        partition_info
            .cpus
            .chunk_by(|a, b| a.vnode == b.vnode)
            .flat_map(|cpus| {
                let chunks = cpus.len().div_ceil(MAX_SIDECAR_NODE_SIZE);
                cpus.chunks(cpus.len().div_ceil(chunks))
            })
    };
    if cpus_by_node().all(|cpus_by_node| cpus_by_node.len() == 1) {
        log!("sidecar: all NUMA nodes have one CPU");
        return None;
    }
    let node_count = cpus_by_node().count();

    let mut total_ram;
    {
        let SidecarParams {
            hypercall_page,
            enable_logging,
            node_count,
            nodes,
        } = sidecar_params;

        *hypercall_page = 0;
        #[cfg(target_arch = "x86_64")]
        {
            *hypercall_page = crate::hypercall::hvcall().hypercall_page();
        }
        *enable_logging = partition_info
            .cmdline
            .split_whitespace()
            .any(|s| s == "SIDECAR_LOGGING=1");

        let mut base_vp = 0;
        total_ram = 0;
        for (cpus, node) in cpus_by_node().zip(nodes) {
            let required_ram = sidecar_defs::required_memory(cpus.len() as u32) as u64;
            // Take some VTL2 RAM for sidecar use. Try to use the same NUMA node
            // as the first CPU.
            let local_vnode = cpus[0].vnode as usize;
            let mut vtl2_ram = &mut free_memory[local_vnode];
            if required_ram >= vtl2_ram.len() {
                // Take RAM from the next NUMA node with enough memory.
                let remote_vnode = free_memory
                    .iter()
                    .enumerate()
                    .cycle()
                    .skip(local_vnode + 1)
                    .take(free_memory.len())
                    .find_map(|(vnode, mem)| (mem.len() >= required_ram).then_some(vnode));
                let Some(remote_vnode) = remote_vnode else {
                    log!("sidecar: not enough memory for sidecar");
                    return None;
                };
                log!("sidecar: not enough memory for sidecar on node {local_vnode}, falling back to node {remote_vnode}");
                vtl2_ram = &mut free_memory[remote_vnode];
            }
            let (rest, mem) = vtl2_ram.split_at_offset(vtl2_ram.len() - required_ram);
            *vtl2_ram = rest;
            *node = SidecarNodeParams {
                memory_base: mem.start(),
                memory_size: mem.len(),
                base_vp,
                vp_count: cpus.len() as u32,
            };
            base_vp += cpus.len() as u32;
            *node_count += 1;
            total_ram += required_ram;
        }
    }

    // SAFETY: the parameter blob is trusted.
    let sidecar_entry: extern "C" fn(&SidecarParams, &mut SidecarOutput) -> bool =
        unsafe { core::mem::transmute(p.sidecar_entry_address) };

    let boot_start_reftime = minimal_rt::reftime::reference_time();
    log!(
        "sidecar starting, {} nodes, {} cpus, {:#x} total bytes",
        node_count,
        partition_info.cpus.len(),
        total_ram
    );
    if !sidecar_entry(sidecar_params, sidecar_output) {
        panic!(
            "failed to start sidecar: {}",
            core::str::from_utf8(&sidecar_output.error.buf[..sidecar_output.error.len as usize])
                .unwrap()
        );
    }
    let boot_end_reftime = minimal_rt::reftime::reference_time();

    let SidecarOutput { nodes, error: _ } = sidecar_output;
    Some(SidecarConfig {
        image,
        start_reftime: boot_start_reftime,
        end_reftime: boot_end_reftime,
        node_params: &sidecar_params.nodes[..node_count],
        nodes: &nodes[..node_count],
    })
}
