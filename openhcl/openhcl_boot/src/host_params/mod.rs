// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Module used to parse the host parameters used to setup Underhill. These are
//! provided via a device tree IGVM parameter.

use crate::cmdline::BootCommandLineOptions;
use crate::host_params::shim_params::IsolationType;
use arrayvec::ArrayString;
use arrayvec::ArrayVec;
use host_fdt_parser::CpuEntry;
use host_fdt_parser::GicInfo;
use host_fdt_parser::MemoryAllocationMode;
use host_fdt_parser::MemoryEntry;
use host_fdt_parser::VmbusInfo;

mod dt;
mod mmio;
pub mod shim_params;

/// Maximum supported cpu count by underhill.
pub const MAX_CPU_COUNT: usize = 2048;

/// The maximum number of supported virtual NUMA nodes. This must be at least as
/// large as whatever the host supports.
pub const MAX_NUMA_NODES: usize = 64;

pub const COMMAND_LINE_SIZE: usize = 0x2000;

/// Each ram range reported by the host for VTL2 is split per NUMA node.
///
/// Today, Hyper-V has a max limit of 64 NUMA nodes, so we should only ever see
/// 64 ram ranges.
pub const MAX_VTL2_RAM_RANGES: usize = 64;

/// The maximum number of ram ranges that can be read from the host.
const MAX_PARTITION_RAM_RANGES: usize = 1024;

/// Maximum size of the host-provided entropy
pub const MAX_ENTROPY_SIZE: usize = 256;

/// Information about the guest partition.
#[derive(Debug)]
pub struct PartitionInfo {
    /// Ram assigned to VTL2. This is either parsed from the host via IGVM
    /// parameters, allocated dynamically, or the fixed at build value.
    ///
    /// This vec is guaranteed to be sorted, and non-overlapping.
    pub vtl2_ram: ArrayVec<MemoryEntry, MAX_VTL2_RAM_RANGES>,
    /// The full memory map provided by the host.
    pub partition_ram: ArrayVec<MemoryEntry, MAX_PARTITION_RAM_RANGES>,
    /// The partiton's isolation type.
    pub isolation: IsolationType,
    /// The reg field in device tree for the BSP. This is either the apic_id on
    /// x64, or mpidr on aarch64.
    pub bsp_reg: u32,
    /// Cpu info for enabled cpus.
    pub cpus: ArrayVec<CpuEntry, MAX_CPU_COUNT>,
    /// Per-CPU state to apply when starting the sidecar kernel.
    pub sidecar_cpu_overrides: sidecar_defs::PerCpuState,
    /// VMBUS info for VTL2.
    pub vmbus_vtl2: VmbusInfo,
    /// VMBUS info for VTL0.
    pub vmbus_vtl0: VmbusInfo,
    /// Command line to be used for the underhill kernel.
    pub cmdline: ArrayString<COMMAND_LINE_SIZE>,
    /// Com3 serial device is available
    pub com3_serial_available: bool,
    /// Memory allocation mode that was performed.
    pub memory_allocation_mode: MemoryAllocationMode,
    /// Entropy from the host to be used by the OpenHCL kernel
    pub entropy: Option<ArrayVec<u8, MAX_ENTROPY_SIZE>>,
    /// The VTL0 alias map physical address.
    pub vtl0_alias_map: Option<u64>,
    /// Host is compatible with DMA preservation / NVMe keep-alive.
    pub nvme_keepalive: bool,
    /// Parsed boot command line options.
    pub boot_options: BootCommandLineOptions,

    /// GIC information on AArch64.
    pub gic: Option<GicInfo>,
    /// PMU GSIV on AArch64.
    pub pmu_gsiv: Option<u32>,
}

impl PartitionInfo {
    /// Create an empty [`PartitionInfo`].
    pub const fn new() -> Self {
        PartitionInfo {
            vtl2_ram: ArrayVec::new_const(),
            partition_ram: ArrayVec::new_const(),
            isolation: IsolationType::None,
            bsp_reg: 0,
            cpus: ArrayVec::new_const(),
            sidecar_cpu_overrides: sidecar_defs::PerCpuState {
                per_cpu_state_specified: false,
                sidecar_starts_cpu: [true; sidecar_defs::NUM_CPUS_SUPPORTED_FOR_PER_CPU_STATE],
            },
            vmbus_vtl2: VmbusInfo {
                mmio: ArrayVec::new_const(),
                connection_id: 0,
            },
            vmbus_vtl0: VmbusInfo {
                mmio: ArrayVec::new_const(),
                connection_id: 0,
            },
            cmdline: ArrayString::new_const(),
            com3_serial_available: false,
            memory_allocation_mode: MemoryAllocationMode::Host,
            entropy: None,
            vtl0_alias_map: None,
            nvme_keepalive: false,
            boot_options: BootCommandLineOptions::new(),
            gic: None,
            pmu_gsiv: None,
        }
    }
}
