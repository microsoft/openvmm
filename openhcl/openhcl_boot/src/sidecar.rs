// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::cmdline::SidecarOptions;
use crate::host_params::MAX_CPU_COUNT;
use crate::host_params::MAX_NUMA_NODES;
use crate::host_params::PartitionInfo;
use crate::host_params::shim_params::IsolationType;
use crate::host_params::shim_params::ShimParams;
use crate::memory::AddressSpaceManager;
use crate::memory::AllocationPolicy;
use crate::memory::AllocationType;
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
    address_space: &mut AddressSpaceManager,
    sidecar_params: &'a mut SidecarParams,
    sidecar_output: &'a mut SidecarOutput,
) -> Option<SidecarConfig<'a>> {
    if !cfg!(target_arch = "x86_64") || p.isolation_type != IsolationType::None {
        return None;
    }

    if p.sidecar_size == 0 {
        log::info!("sidecar: not present in image");
        return None;
    }

    match partition_info.boot_options.sidecar {
        SidecarOptions::DisabledCommandLine => {
            log::info!("sidecar: disabled via command line");
            return None;
        }
        SidecarOptions::DisabledServicing => {
            log::info!("sidecar: disabled because this is a servicing restore");
            return None;
        }
        SidecarOptions::Enabled { enable_logging, .. } => {
            sidecar_params.enable_logging = enable_logging;
        }
    }

    // Ensure the host didn't provide an out-of-bounds NUMA node.
    let max_vnode = partition_info
        .cpus
        .iter()
        .map(|cpu| cpu.vnode)
        .chain(partition_info.vtl2_ram.iter().map(|e| e.vnode))
        .max()
        .unwrap();

    if max_vnode >= MAX_NUMA_NODES as u32 {
        log::warn!("sidecar: NUMA node {max_vnode} too large");
        return None;
    }

    #[cfg(target_arch = "x86_64")]
    if !x86defs::cpuid::VersionAndFeaturesEcx::from(
        safe_intrinsics::cpuid(x86defs::cpuid::CpuidFunction::VersionAndFeatures.0, 0).ecx,
    )
    .x2_apic()
    {
        // Currently, sidecar needs x2apic to communicate with the kernel
        log::warn!("sidecar: x2apic not available; not using sidecar");
        return None;
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
        log::info!("sidecar: all NUMA nodes have one CPU");
        return None;
    }
    let node_count = cpus_by_node().count();

    let mut total_ram;
    {
        let SidecarParams {
            hypercall_page,
            enable_logging: _,
            node_count,
            nodes,
        } = sidecar_params;

        *hypercall_page = 0;
        #[cfg(target_arch = "x86_64")]
        {
            *hypercall_page = crate::hypercall::hvcall().hypercall_page();
        }

        let mut base_vp = 0;
        total_ram = 0;
        for (cpus, node) in cpus_by_node().zip(nodes) {
            let required_ram = sidecar_defs::required_memory(cpus.len() as u32) as u64;
            // Take some VTL2 RAM for sidecar use. Try to use the same NUMA node
            // as the first CPU.
            let local_vnode = cpus[0].vnode as usize;

            let mem = match address_space.allocate(
                Some(local_vnode as u32),
                required_ram,
                AllocationType::SidecarNode,
                AllocationPolicy::LowMemory,
            ) {
                Some(mem) => mem,
                None => {
                    // Fallback to no numa requirement.
                    match address_space.allocate(
                        None,
                        required_ram,
                        AllocationType::SidecarNode,
                        AllocationPolicy::LowMemory,
                    ) {
                        Some(mem) => {
                            log::warn!(
                                "sidecar: unable to allocate memory for sidecar node on node {local_vnode}, falling back to node {}",
                                mem.vnode
                            );
                            mem
                        }
                        None => {
                            log::warn!("sidecar: not enough memory for sidecar");
                            return None;
                        }
                    }
                }
            };

            *node = SidecarNodeParams {
                memory_base: mem.range.start(),
                memory_size: mem.range.len(),
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
    log::info!(
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
        start_reftime: boot_start_reftime,
        end_reftime: boot_end_reftime,
        node_params: &sidecar_params.nodes[..node_count],
        nodes: &nodes[..node_count],
    })
}
