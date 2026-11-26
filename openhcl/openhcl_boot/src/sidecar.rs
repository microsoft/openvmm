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
    pub num_cpus: usize,
    pub per_cpu_state: &'a sidecar_defs::PerCpuState,
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
        // Generate boot_cpus= parameter listing CPUs that Linux should start
        // directly (all others will be managed by sidecar).
        // When per-CPU overrides are active (servicing restore with outstanding IO),
        // list every CPU that sidecar should NOT start.
        // Otherwise, list sidecar node base VPs and CPUs outside retained nodes.
        f.write_str("boot_cpus=")?;
        let mut comma = "";
        if self.0.per_cpu_state.per_cpu_state_specified {
            for (i, &starts) in self.0.per_cpu_state.sidecar_starts_cpu[..self.0.num_cpus]
                .iter()
                .enumerate()
            {
                if !starts {
                    write!(f, "{comma}{i}")?;
                    comma = ",";
                }
            }
        } else {
            for cpu in 0..self.0.num_cpus {
                let sidecar_starts_cpu = self.0.node_params.iter().any(|node| {
                    let base_vp = node.base_vp as usize;
                    cpu > base_vp && cpu - base_vp < node.vp_count as usize
                });
                if !sidecar_starts_cpu {
                    write!(f, "{comma}{cpu}")?;
                    comma = ",";
                }
            }
        }
        Ok(())
    }
}

/// Returns true if, with per-CPU sidecar overrides active, the sidecar node
/// whose VPs are `base_vp..base_vp + size` has no application processors left
/// for sidecar to start (every AP is kernel-started).
///
/// The first VP of a node is its base VP and is always kernel-started, so
/// sidecar only ever starts the remaining VPs.
fn sidecar_node_is_empty(
    overrides: &sidecar_defs::PerCpuState,
    base_vp: usize,
    size: usize,
) -> bool {
    // Without per-CPU overrides sidecar starts every non-base VP.
    if !overrides.per_cpu_state_specified {
        return size == 1;
    }
    // Empty iff no non-base VP is left for sidecar to start.
    !(1..size).any(|i| overrides.sidecar_starts_cpu[base_vp + i])
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
        SidecarOptions::DisabledPerCpuStateCapacity => {
            log::info!(
                "sidecar: disabled because servicing per-CPU overrides exceed state capacity"
            );
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
    let mut total_ram;
    {
        let SidecarParams {
            hypercall_page,
            enable_logging: _,
            node_count,
            nodes,
            initial_state,
        } = sidecar_params;

        *hypercall_page = 0;
        #[cfg(target_arch = "x86_64")]
        {
            *hypercall_page = crate::hypercall::hvcall().hypercall_page();
        }

        let mut base_vp = 0;
        total_ram = 0;
        *node_count = 0;
        *initial_state = partition_info.sidecar_cpu_overrides.clone();
        for cpus in cpus_by_node() {
            // Skip creating a sidecar node when none of its application
            // processors are left for sidecar to start (all are kernel-started).
            if sidecar_node_is_empty(
                &partition_info.sidecar_cpu_overrides,
                base_vp as usize,
                cpus.len(),
            ) {
                if initial_state.per_cpu_state_specified {
                    // Kernel-start the base VP too; its APs are already
                    // excluded, so every VP in the node ends up in `boot_cpus=`.
                    initial_state.sidecar_starts_cpu[base_vp as usize] = false;
                }
                log::info!(
                    "sidecar: node at base VP {base_vp} ({} VPs) has no sidecar-started APs; kernel-starting all of them",
                    cpus.len(),
                );
                base_vp += cpus.len() as u32;
                continue;
            }

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

            nodes[*node_count as usize] = SidecarNodeParams {
                memory_base: mem.range.start(),
                memory_size: mem.range.len(),
                base_vp,
                vp_count: cpus.len() as u32,
            };
            log::info!(
                "sidecar: created node index={} base_vp={base_vp} vp_count={} vnode={local_vnode}",
                *node_count,
                cpus.len(),
            );
            if initial_state.per_cpu_state_specified {
                // If per-CPU state is specified, make sure to explicitly state that
                // sidecar should not start the base vp of this node.
                // The code that set per_cpu_state_specified should have already ensured that
                // the array is large enough for any `base_vp` we might have here.
                initial_state.sidecar_starts_cpu[base_vp as usize] = false;
                log::info!(
                    "sidecar: per_cpu_state_specified=true, marking base_vp={} as kernel-started",
                    base_vp
                );
            }
            base_vp += cpus.len() as u32;
            *node_count += 1;
            total_ram += required_ram;
        }
    }

    // If per-CPU overrides left every node empty, there is nothing for
    // sidecar to do; behave as if it were disabled entirely.
    let node_count = sidecar_params.node_count as usize;
    if node_count == 0 {
        log::info!("sidecar: no nodes have sidecar-started APs; disabling sidecar");
        return None;
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
    let config = SidecarConfig {
        num_cpus: partition_info.cpus.len(),
        start_reftime: boot_start_reftime,
        end_reftime: boot_end_reftime,
        node_params: &sidecar_params.nodes[..node_count],
        nodes: &nodes[..node_count],
        per_cpu_state: &sidecar_params.initial_state,
    };
    log::info!(
        "sidecar: boot_cpus parameter: {}",
        config.kernel_command_line()
    );
    Some(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidecar_defs::PerCpuState;
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    fn command_line(
        num_cpus: usize,
        per_cpu_state: &PerCpuState,
        sidecar_ranges: &[(u32, u32)],
    ) -> String {
        let node_params: Vec<_> = sidecar_ranges
            .iter()
            .map(|&(base_vp, vp_count)| SidecarNodeParams {
                base_vp,
                vp_count,
                ..SidecarNodeParams::new_zeroed()
            })
            .collect();
        let nodes: Vec<_> = sidecar_ranges
            .iter()
            .map(|_| SidecarNodeOutput::new_zeroed())
            .collect();
        let config = SidecarConfig {
            num_cpus,
            per_cpu_state,
            node_params: &node_params,
            nodes: &nodes,
            start_reftime: 0,
            end_reftime: 0,
        };
        config.kernel_command_line().to_string()
    }

    // Formats cold-boot assignment for sidecar CPUs 0-1 and a skipped singleton CPU 2.
    // Expects Linux to start CPUs 0 and 2 while sidecar retains application processor 1.
    #[test]
    fn test_kernel_command_line_includes_singleton_cpu() {
        let per_cpu_state = PerCpuState::new_zeroed();
        assert!(sidecar_node_is_empty(&per_cpu_state, 2, 1));

        assert_eq!(command_line(3, &per_cpu_state, &[(0, 2)]), "boot_cpus=0,2");
    }

    // Formats cold-boot assignment when both CPUs belong to the same sidecar node.
    // Expects only the base CPU in the Linux list, leaving its AP to sidecar.
    #[test]
    fn test_kernel_command_line_keeps_sidecar_application_processor() {
        let per_cpu_state = PerCpuState::new_zeroed();
        assert_eq!(command_line(2, &per_cpu_state, &[(0, 2)]), "boot_cpus=0");
    }

    // Formats the same mixed topology with explicit per-CPU startup overrides.
    // Expects Linux CPUs 0 and 2 and excludes the sidecar-started CPU 1.
    #[test]
    fn test_kernel_command_line_respects_per_cpu_overrides() {
        let mut per_cpu_state = PerCpuState {
            per_cpu_state_specified: true,
            ..PerCpuState::new_zeroed()
        };
        per_cpu_state.sidecar_starts_cpu[1] = true;
        assert_eq!(command_line(3, &per_cpu_state, &[(0, 2)]), "boot_cpus=0,2");
    }

    // Formats two sidecar nodes with kernel-only CPUs between and after their ranges.
    // Expects every base and gap CPU in the Linux list, excluding both sidecar APs.
    #[test]
    fn test_kernel_command_line_includes_gaps_between_sidecar_nodes() {
        let per_cpu_state = PerCpuState::new_zeroed();
        assert_eq!(
            command_line(6, &per_cpu_state, &[(0, 2), (3, 2)]),
            "boot_cpus=0,2,3,5"
        );
    }

    // Formats prepared cold-boot configurations with skipped CPUs and larger nodes.
    // Expects the exact Linux CPU list, leaving every sidecar AP out of that list.
    #[test]
    fn test_kernel_command_line_cold_boot_configurations() {
        let ranges_1024: Vec<_> = (0..32).map(|i| (i * 32, 32)).collect();
        let ranges_3000: Vec<_> = (0..30)
            .map(|i| (i * 32, 32))
            .chain((0..31).map(|i| (960 + i * 32, 32)))
            .chain([(1952, 8)])
            .chain((0..32).map(|i| (1960 + i * 32, 32)))
            .chain([(2984, 16)])
            .collect();
        let ranges_mixed: Vec<_> = (0..15)
            .map(|i| (2 + i * 32, 32))
            .chain((0..16).map(|i| (485 + i * 32, 32)))
            .collect();
        let cases: &[(&str, usize, &[(u32, u32)], &str)] = &[
            ("leading singletons", 5, &[(2, 3)], "boot_cpus=0,1,2"),
            (
                "consecutive singletons between and after nodes",
                10,
                &[(0, 2), (5, 3)],
                "boot_cpus=0,2,3,4,5,8,9",
            ),
            ("31 CPUs", 31, &[(0, 31)], "boot_cpus=0"),
            ("32 CPUs", 32, &[(0, 32)], "boot_cpus=0"),
            (
                "33 CPUs in two nodes",
                33,
                &[(0, 17), (17, 16)],
                "boot_cpus=0,17",
            ),
            (
                "adjacent 32-CPU nodes",
                64,
                &[(0, 32), (32, 32)],
                "boot_cpus=0,32",
            ),
            (
                "96 CPUs across three NUMA nodes",
                96,
                &[(0, 32), (32, 32), (64, 32)],
                "boot_cpus=0,32,64",
            ),
            (
                "1024 CPUs with many adjacent sidecar nodes",
                1024,
                &ranges_1024,
                concat!(
                    "boot_cpus=0,32,64,96,128,160,192,224,256,288,320,352,384,416,448,480,",
                    "512,544,576,608,640,672,704,736,768,800,832,864,896,928,960,992",
                ),
            ),
            (
                "3000 CPUs across uneven 960/1000/1040-CPU NUMA nodes",
                3000,
                &ranges_3000,
                concat!(
                    "boot_cpus=0,32,64,96,128,160,192,224,256,288,320,352,384,416,448,480,",
                    "512,544,576,608,640,672,704,736,768,800,832,864,896,928,",
                    "960,992,1024,1056,1088,1120,1152,1184,1216,1248,1280,1312,1344,1376,",
                    "1408,1440,1472,1504,1536,1568,1600,1632,1664,1696,1728,1760,1792,",
                    "1824,1856,1888,1920,1952,",
                    "1960,1992,2024,2056,2088,2120,2152,2184,2216,2248,2280,2312,2344,",
                    "2376,2408,2440,2472,2504,2536,2568,2600,2632,2664,2696,2728,2760,",
                    "2792,2824,2856,2888,2920,2952,2984",
                ),
            ),
            (
                "1000 CPUs with singleton nodes before, between, and after sidecar nodes",
                1000,
                &ranges_mixed,
                concat!(
                    "boot_cpus=0,1,2,34,66,98,130,162,194,226,258,290,322,354,386,418,450,",
                    "482,483,484,485,517,549,581,613,645,677,709,741,773,805,837,869,901,",
                    "933,965,997,998,999",
                ),
            ),
        ];
        let per_cpu_state = PerCpuState::new_zeroed();
        for &(name, num_cpus, ranges, expected) in cases {
            assert_eq!(
                command_line(num_cpus, &per_cpu_state, ranges),
                expected,
                "{name}"
            );
        }
    }

    // Formats multi-node configurations with explicit kernel/sidecar CPU ownership.
    // Expects all kernel-owned CPUs, including APs, and excludes only sidecar-owned CPUs.
    #[test]
    fn test_kernel_command_line_multi_node_overrides() {
        let cases: &[(&str, &[(u32, u32)], &[usize], &str)] = &[
            (
                "kernel and sidecar APs in both nodes",
                &[(0, 4), (4, 4)],
                &[2, 5],
                "boot_cpus=0,1,3,4,6,7",
            ),
            (
                "first NUMA node entirely kernel-owned",
                &[(4, 4)],
                &[5, 7],
                "boot_cpus=0,1,2,3,4,6",
            ),
        ];
        for &(name, ranges, sidecar_cpus, expected) in cases {
            let mut per_cpu_state = PerCpuState {
                per_cpu_state_specified: true,
                ..PerCpuState::new_zeroed()
            };
            for &cpu in sidecar_cpus {
                per_cpu_state.sidecar_starts_cpu[cpu] = true;
            }
            assert_eq!(command_line(8, &per_cpu_state, ranges), expected, "{name}");
        }
    }
}
