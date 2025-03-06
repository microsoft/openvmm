// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Module used to parse the host parameters used to setup Underhill. These are
//! provided via a device tree IGVM parameter.

use crate::IsolationType;
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

/// Maximum size of the host-provided entropy
pub const MAX_ENTROPY_SIZE: usize = 256;

/// Maximum number of supported VTL2 used ranges.
pub const MAX_VTL2_USED_RANGES: usize = 16;

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
    /// The vtl2 reserved region, that is reserved to both the kernel and
    /// usermode.
    pub vtl2_reserved_region: MemoryRange,
    /// Memory used for the VTL2 private pool.
    pub vtl2_pool_memory: MemoryRange,
    /// Memory ranges that are in use by the bootshim, and any other persisted
    /// ranges, such as the VTL2 private pool.
    ///
    /// TODO: Refactor these different ranges and consolidate address space
    /// management.
    pub vtl2_used_ranges: ArrayVec<MemoryRange, MAX_VTL2_USED_RANGES>,
    ///  The full memory map provided by the host.
    pub partition_ram: ArrayVec<MemoryEntry, MAX_PARTITION_RAM_RANGES>,
    /// The partiton's isolation type.
    pub isolation: IsolationType,
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
    /// Entropy from the host to be used by the OpenHCL kernel
    pub entropy: Option<ArrayVec<u8, MAX_ENTROPY_SIZE>>,
    /// The VTL0 alias map physical address.
    pub vtl0_alias_map: Option<u64>,
    /// Host is compatible with DMA preservation / NVMe keep-alive.
    pub nvme_keepalive: bool,
}

impl PartitionInfo {
    /// Create an empty [`PartitionInfo`].
    pub const fn new() -> Self {
        PartitionInfo {
            vtl2_ram: ArrayVec::new_const(),
            vtl2_full_config_region: MemoryRange::EMPTY,
            vtl2_config_region_reclaim: MemoryRange::EMPTY,
            vtl2_reserved_region: MemoryRange::EMPTY,
            vtl2_pool_memory: MemoryRange::EMPTY,
            vtl2_used_ranges: ArrayVec::new_const(),
            partition_ram: ArrayVec::new_const(),
            isolation: IsolationType::None,
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
            entropy: None,
            vtl0_alias_map: None,
            nvme_keepalive: false,
        }
    }

    /// Returns the parameter regions that are not being reclaimed.
    pub fn vtl2_config_regions(&self) -> impl Iterator<Item = MemoryRange> + use<> {
        subtract_ranges(
            [self.vtl2_full_config_region],
            [self.vtl2_config_region_reclaim],
        )
    }
}
