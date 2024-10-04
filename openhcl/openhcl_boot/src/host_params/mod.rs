// Copyright (C) Microsoft Corporation. All rights reserved.

//! Module used to parse the host parameters used to setup Underhill. These are
//! provided via a device tree IGVM parameter.

use arrayvec::ArrayString;
use arrayvec::ArrayVec;
use host_fdt_parser::CpuEntry;
use host_fdt_parser::GicInfo;
use host_fdt_parser::MemoryAllocationMode;
use host_fdt_parser::MemoryEntry;
use host_fdt_parser::VmbusInfo;
use memory_range::subtract_ranges;
use memory_range::MemoryRange;

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
const MAX_VTL2_RAM_RANGES: usize = 64;

/// The maximum number of ram ranges that can be read from the host.
const MAX_PARTITION_RAM_RANGES: usize = 1024;

/// Information about the guest partition.
#[derive(Debug)]
pub struct PartitionInfo {
    /// Ram assigned to VTL2. This is either parsed from the host via IGVM
    /// parameters, or the fixed at build value.
    ///
    /// This vec is guaranteed to be sorted, and non-overlapping.
    pub vtl2_ram: ArrayVec<MemoryEntry, MAX_VTL2_RAM_RANGES>,
    /// The parameter region.
    pub vtl2_full_config_region: MemoryRange,
    /// Additional ram that can be reclaimed from the parameter region. Today,
    /// this is the whole device tree provided by the host.
    pub vtl2_config_region_reclaim: MemoryRange,
    /// The full memory map provided by the host.
    pub partition_ram: ArrayVec<MemoryEntry, MAX_PARTITION_RAM_RANGES>,
    /// The reg field in device tree for the BSP. This is either the apic_id on
    /// x64, or mpidr on aarch64.
    pub bsp_reg: u32,
    /// Cpu info for enabled cpus.
    pub cpus: ArrayVec<CpuEntry, MAX_CPU_COUNT>,
    /// VMBUS info for VTL2.
    pub vmbus_vtl2: VmbusInfo,
    /// VMBUS info for VTL0.
    pub vmbus_vtl0: VmbusInfo,
    /// Command line to be used for the underhill kernel.
    pub cmdline: ArrayString<COMMAND_LINE_SIZE>,
    /// Com3 serial device is available
    pub com3_serial_available: bool,
    /// GIC information
    pub gic: Option<GicInfo>,
    /// Memory allocation mode that was performed.
    pub memory_allocation_mode: MemoryAllocationMode,
}

impl PartitionInfo {
    /// Create an empty [`PartitionInfo`].
    pub const fn new() -> Self {
        PartitionInfo {
            vtl2_ram: ArrayVec::new_const(),
            vtl2_full_config_region: MemoryRange::EMPTY,
            vtl2_config_region_reclaim: MemoryRange::EMPTY,
            partition_ram: ArrayVec::new_const(),
            bsp_reg: 0,
            cpus: ArrayVec::new_const(),
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
            gic: None,
            memory_allocation_mode: MemoryAllocationMode::Host,
        }
    }

    /// Returns the parameter regions that are not being reclaimed.
    pub fn vtl2_config_regions(&self) -> impl Iterator<Item = MemoryRange> {
        subtract_ranges(
            [self.vtl2_full_config_region],
            [self.vtl2_config_region_reclaim],
        )
    }
}
