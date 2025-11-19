// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parsing code for the devicetree provided by openhcl_boot used by underhill
//! usermode. Unlike `host_fdt_parser`, this code requires std as it is intended
//! to be only used in usermode.

#![forbid(unsafe_code)]

use anyhow::Context;
use anyhow::bail;
use fdt::parser::Node;
use fdt::parser::Parser;
use fdt::parser::Property;
use igvm_defs::MemoryMapEntryType;
use igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY;
use inspect::Inspect;
use loader_defs::shim::MemoryVtlType;
use memory_range::MemoryRange;
use vm_topology::memory::MemoryRangeWithNode;
use vm_topology::processor::aarch64::GicInfo;

/// A parsed cpu.
#[derive(Debug, Inspect, Clone, Copy, PartialEq, Eq)]
pub struct Cpu {
    /// Architecture specific "reg" value for this CPU. For x64, this is the
    /// APIC ID. For ARM v8 64-bit, this should match the MPIDR_EL1 register
    /// affinity bits.
    pub reg: u64,
    /// The vnode field of a cpu dt node, which describes the numa node id.
    pub vnode: u32,
}

/// Information about a guest memory range.
#[derive(Debug, Inspect, Clone, PartialEq, Eq)]
pub struct Memory {
    /// The range of memory.
    pub range: MemoryRangeWithNode,
    /// The VTL this memory is for.
    #[inspect(debug)]
    pub vtl_usage: MemoryVtlType,
    /// The host provided IGVM type for this memory.
    #[inspect(debug)]
    pub igvm_type: MemoryMapEntryType,
}

/// Vtls for mmio.
#[derive(Debug, Inspect, Clone, PartialEq, Eq)]
pub enum Vtl {
    /// VTL0.
    Vtl0,
    /// VTL2.
    Vtl2,
}

/// Information about guest mmio.
#[derive(Debug, Inspect, Clone, PartialEq, Eq)]
pub struct Mmio {
    /// The address range of mmio.
    pub range: MemoryRange,
    /// The VTL this mmio is for.
    pub vtl: Vtl,
}

/// Information about a section of the guest's address space.
#[derive(Debug, Inspect, Clone, PartialEq, Eq)]
#[inspect(tag = "type")]
pub enum AddressRange {
    /// This range describes memory.
    Memory(#[inspect(flatten)] Memory),
    /// This range describes mmio.
    Mmio(#[inspect(flatten)] Mmio),
}

impl AddressRange {
    /// The [`MemoryRange`] for this address range.
    pub fn range(&self) -> &MemoryRange {
        match self {
            AddressRange::Memory(memory) => &memory.range.range,
            AddressRange::Mmio(mmio) => &mmio.range,
        }
    }

    /// The [`MemoryVtlType`] for this address range.
    pub fn vtl_usage(&self) -> MemoryVtlType {
        match self {
            AddressRange::Memory(memory) => memory.vtl_usage,
            AddressRange::Mmio(Mmio { vtl, .. }) => match vtl {
                Vtl::Vtl0 => MemoryVtlType::VTL0_MMIO,
                Vtl::Vtl2 => MemoryVtlType::VTL2_MMIO,
            },
        }
    }
}

/// The isolation type of the partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Inspect)]
pub enum IsolationType {
    /// No isolation.
    None,
    /// Hyper-V based isolation.
    Vbs,
    /// AMD SNP.
    Snp,
    /// Intel TDX.
    Tdx,
}

/// The memory allocation mode provided by the host. This reports how the
/// bootloader decided to provide memory for the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Inspect)]
#[inspect(external_tag)]
pub enum MemoryAllocationMode {
    /// Use the host provided memory topology, and use VTL2_PROTECTABLE entries
    /// as VTL2 ram. This is the default if no
    /// `openhcl/memory-allocation-property` mode is provided by the host.
    Host,
    /// Allow VTL2 to select its own ranges from the address space to use for
    /// memory, with a size provided by the host.
    Vtl2 {
        /// The number of bytes VTL2 should allocate for memory for itself.
        /// Encoded as `openhcl/memory-size` in device tree.
        #[inspect(hex)]
        memory_size: Option<u64>,
        /// The number of bytes VTL2 should allocate for mmio for itself.
        /// Encoded as `openhcl/mmio-size` in device tree.
        #[inspect(hex)]
        mmio_size: Option<u64>,
    },
}

/// Information parsed from the device tree provided by openhcl_boot. These
/// values are trusted, as it's expected that openhcl_boot has already validated
/// the host provided device tree.
#[derive(Debug, Inspect, PartialEq, Eq)]
pub struct ParsedBootDtInfo {
    /// The cpus in the system. The index in the vector is also the mshv VP
    /// index.
    #[inspect(iter_by_index)]
    pub cpus: Vec<Cpu>,
    /// The physical address of the VTL0 alias mapping, if one is configured.
    pub vtl0_alias_map: Option<u64>,
    /// The memory ranges for VTL2 that were reported to the kernel. This is
    /// sorted in ascending order.
    #[inspect(iter_by_index)]
    pub vtl2_memory: Vec<MemoryRangeWithNode>,
    /// The unified memory map for the partition, from the bootloader. Sorted in
    /// ascending order. Note that this includes mmio gaps as well.
    #[inspect(with = "inspect_helpers::memory_internal")]
    pub partition_memory_map: Vec<AddressRange>,
    /// The mmio to report to VTL0.
    #[inspect(iter_by_index)]
    pub vtl0_mmio: Vec<MemoryRange>,
    /// The ranges config regions are stored at.
    #[inspect(iter_by_index)]
    pub config_ranges: Vec<MemoryRange>,
    /// The VTL2 reserved range.
    pub vtl2_reserved_range: MemoryRange,
    /// The memory range that contains the persisted header describing the
    /// persisted protobuf region.
    pub vtl2_persisted_header: MemoryRange,
    /// The memory range that contains the persisted protobuf region.
    pub vtl2_persisted_protobuf_region: MemoryRange,
    /// The ranges that were accepted at load time by the host on behalf of the
    /// guest.
    #[inspect(iter_by_index)]
    pub accepted_ranges: Vec<MemoryRange>,
    /// The memory allocation mode the bootloader decided to use.
    pub memory_allocation_mode: MemoryAllocationMode,
    /// The isolation type of the partition.
    pub isolation: IsolationType,
    /// VTL2 range for private pool memory.
    #[inspect(iter_by_index)]
    pub private_pool_ranges: Vec<MemoryRangeWithNode>,

    /// GIC information, on AArch64.
    pub gic: Option<GicInfo>,
    /// PMU GSIV, on AArch64.
    pub pmu_gsiv: Option<u32>,
}

fn err_to_owned(e: fdt::parser::Error<'_>) -> anyhow::Error {
    anyhow::Error::msg(e.to_string())
}

/// Try to find a given property on a node, returning None if not found. As the
/// bootloader should be producing a well formed device tree, errors are not
/// expected and flattened into `None`.
fn try_find_property<'a>(node: &Node<'a>, name: &str) -> Option<Property<'a>> {
    node.find_property(name).ok().flatten()
}

fn address_cells(node: &Node<'_>) -> anyhow::Result<u32> {
    let prop = try_find_property(node, "#address-cells")
        .context("missing address cells on {node.name}")?;
    prop.read_u32(0).map_err(err_to_owned)
}

fn property_to_u64_vec(node: &Node<'_>, name: &str) -> anyhow::Result<Vec<u64>> {
    let prop = try_find_property(node, name).context("missing prop {name} on {node.name}")?;
    Ok(prop
        .as_64_list()
        .map_err(err_to_owned)
        .context("prop {name} is not a list of u64s")?
        .collect())
}

struct OpenhclInfo {
    vtl0_mmio: Vec<MemoryRange>,
    config_ranges: Vec<MemoryRange>,
    partition_memory_map: Vec<AddressRange>,
    accepted_memory: Vec<MemoryRange>,
    vtl2_reserved_range: MemoryRange,
    vtl0_alias_map: Option<u64>,
    memory_allocation_mode: MemoryAllocationMode,
    isolation: IsolationType,
    private_pool_ranges: Vec<MemoryRangeWithNode>,
    vtl2_persisted_header: MemoryRange,
    vtl2_persisted_protobuf_region: MemoryRange,
}

fn parse_memory_openhcl(node: &Node<'_>) -> anyhow::Result<AddressRange> {
    let vtl_usage = {
        let prop = try_find_property(node, "openhcl,memory-type")
            .context(format!("missing openhcl,memory-type on node {}", node.name))?;

        MemoryVtlType(prop.read_u32(0).map_err(err_to_owned).context(format!(
            "openhcl memory node {} openhcl,memory-type invalid",
            node.name
        ))?)
    };

    if vtl_usage.ram() {
        // Parse this entry as memory.
        let range = parse_memory(node).context("unable to parse base memory")?;

        let igvm_type = {
            let prop = try_find_property(node, IGVM_DT_IGVM_TYPE_PROPERTY)
                .context(format!("missing igvm type on node {}", node.name))?;
            let value = prop
                .read_u32(0)
                .map_err(err_to_owned)
                .context(format!("memory node {} invalid igvm type", node.name))?;
            MemoryMapEntryType(value as u16)
        };

        Ok(AddressRange::Memory(Memory {
            range,
            vtl_usage,
            igvm_type,
        }))
    } else {
        // Parse this type as just mmio.
        let range = {
            let reg = property_to_u64_vec(node, "reg")?;

            if reg.len() != 2 {
                bail!("mmio node {} does not have 2 u64s", node.name);
            }

            let base = reg[0];
            let len = reg[1];
            MemoryRange::try_new(base..(base + len)).context("invalid mmio range")?
        };

        let vtl = match vtl_usage {
            MemoryVtlType::VTL0_MMIO => Vtl::Vtl0,
            MemoryVtlType::VTL2_MMIO => Vtl::Vtl2,
            _ => bail!(
                "invalid vtl_usage {vtl_usage:?} type for mmio node {}",
                node.name
            ),
        };

        Ok(AddressRange::Mmio(Mmio { range, vtl }))
    }
}

fn parse_accepted_memory(node: &Node<'_>) -> anyhow::Result<MemoryRange> {
    let reg = property_to_u64_vec(node, "reg")?;

    if reg.len() != 2 {
        bail!("accepted memory node {} does not have 2 u64s", node.name);
    }

    let base = reg[0];
    let len = reg[1];
    MemoryRange::try_new(base..(base + len)).context("invalid preaccepted memory")
}

fn parse_openhcl(node: &Node<'_>) -> anyhow::Result<OpenhclInfo> {
    let mut memory = Vec::new();
    let mut accepted_memory = Vec::new();

    for child in node.children() {
        let child = child.map_err(err_to_owned).context("child invalid")?;

        match child.name {
            name if name.starts_with("memory@") => {
                memory.push(parse_memory_openhcl(&child)?);
            }

            name if name.starts_with("accepted-memory@") => {
                accepted_memory.push(parse_accepted_memory(&child)?);
            }

            name if name.starts_with("memory-allocation-mode") => {}

            _ => {
                // Ignore other nodes.
            }
        }
    }

    let isolation = {
        let prop = try_find_property(node, "isolation-type").context("missing isolation-type")?;

        match prop.read_str().map_err(err_to_owned)? {
            "none" => IsolationType::None,
            "vbs" => IsolationType::Vbs,
            "snp" => IsolationType::Snp,
            "tdx" => IsolationType::Tdx,
            ty => bail!("invalid isolation-type {ty}"),
        }
    };

    let memory_allocation_mode = {
        let prop = try_find_property(node, "memory-allocation-mode")
            .context("missing memory-allocation-mode")?;

        match prop.read_str().map_err(err_to_owned)? {
            "host" => MemoryAllocationMode::Host,
            "vtl2" => {
                let memory_size = try_find_property(node, "memory-size")
                    .map(|p| p.read_u64(0))
                    .transpose()
                    .map_err(err_to_owned)?;

                let mmio_size = try_find_property(node, "mmio-size")
                    .map(|p| p.read_u64(0))
                    .transpose()
                    .map_err(err_to_owned)?;

                MemoryAllocationMode::Vtl2 {
                    memory_size,
                    mmio_size,
                }
            }
            mode => bail!("invalid memory-allocation-mode {mode}"),
        }
    };

    memory.sort_by_key(|r| r.range().start());
    accepted_memory.sort_by_key(|r| r.start());

    // Report config ranges in a separate vec as well, for convenience.
    let config_ranges = memory
        .iter()
        .filter_map(|entry| {
            if entry.vtl_usage() == MemoryVtlType::VTL2_CONFIG {
                Some(*entry.range())
            } else {
                None
            }
        })
        .collect();

    // Report the reserved range. There should only be one.
    let vtl2_reserved_range = {
        let mut reserved_range_iter = memory.iter().filter_map(|entry| {
            if entry.vtl_usage() == MemoryVtlType::VTL2_RESERVED {
                Some(*entry.range())
            } else {
                None
            }
        });

        let reserved_range = reserved_range_iter.next().unwrap_or(MemoryRange::EMPTY);

        if reserved_range_iter.next().is_some() {
            bail!("multiple VTL2 reserved ranges found");
        }

        reserved_range
    };

    // Report private pool ranges in a separate vec, for convenience.
    let private_pool_ranges = memory
        .iter()
        .filter_map(|entry| match entry {
            AddressRange::Memory(memory) => {
                if memory.vtl_usage == MemoryVtlType::VTL2_GPA_POOL {
                    Some(memory.range.clone())
                } else {
                    None
                }
            }
            AddressRange::Mmio(_) => None,
        })
        .collect();

    // Report the persisted header range.
    let vtl2_persisted_header = {
        let mut header_iter = memory.iter().filter_map(|entry| {
            if entry.vtl_usage() == MemoryVtlType::VTL2_PERSISTED_STATE_HEADER {
                Some(*entry.range())
            } else {
                None
            }
        });

        let header = header_iter
            .next()
            .ok_or(anyhow::anyhow!("no VTL2 persisted header range found"))?;

        if header_iter.next().is_some() {
            bail!("multiple VTL2 persisted header ranges found");
        }

        header
    };

    // Report the persisted protobuf region range.
    let vtl2_persisted_protobuf_region = {
        let mut region_iter = memory.iter().filter_map(|entry| {
            if entry.vtl_usage() == MemoryVtlType::VTL2_PERSISTED_STATE_PROTOBUF {
                Some(*entry.range())
            } else {
                None
            }
        });

        let region = region_iter.next().ok_or(anyhow::anyhow!(
            "no VTL2 persisted protobuf region range found"
        ))?;

        if region_iter.next().is_some() {
            bail!("multiple VTL2 persisted protobuf region ranges found");
        }

        region
    };

    let vtl0_alias_map = try_find_property(node, "vtl0-alias-map")
        .map(|prop| prop.read_u64(0).map_err(err_to_owned))
        .transpose()
        .context("unable to read vtl0-alias-map")?;

    // Extract vmbus mmio information from the overall memory map.
    let vtl0_mmio = memory
        .iter()
        .filter_map(|range| match range {
            AddressRange::Memory(_) => None,
            AddressRange::Mmio(mmio) => match mmio.vtl {
                Vtl::Vtl0 => Some(mmio.range),
                Vtl::Vtl2 => None,
            },
        })
        .collect();

    Ok(OpenhclInfo {
        vtl0_mmio,
        config_ranges,
        partition_memory_map: memory,
        accepted_memory,
        vtl2_reserved_range,
        vtl0_alias_map,
        memory_allocation_mode,
        isolation,
        private_pool_ranges,
        vtl2_persisted_header,
        vtl2_persisted_protobuf_region,
    })
}

fn parse_cpus(node: &Node<'_>) -> anyhow::Result<Vec<Cpu>> {
    let address_cells = address_cells(node)?;

    if address_cells > 2 {
        bail!("cpus address-cells > 2 unexpected");
    }

    let mut cpus = Vec::new();

    for cpu in node.children() {
        let cpu = cpu.map_err(err_to_owned).context("cpu invalid")?;
        let reg = try_find_property(&cpu, "reg").context("{cpu.name} missing reg")?;

        let reg = match address_cells {
            1 => reg.read_u32(0).map_err(err_to_owned)? as u64,
            2 => reg.read_u64(0).map_err(err_to_owned)?,
            _ => unreachable!(),
        };

        let vnode = try_find_property(&cpu, "numa-node-id")
            .context("{cpu.name} missing numa-node-id")?
            .read_u32(0)
            .map_err(err_to_owned)?;

        cpus.push(Cpu { reg, vnode });
    }

    Ok(cpus)
}

/// Parse a single memory node.
fn parse_memory(node: &Node<'_>) -> anyhow::Result<MemoryRangeWithNode> {
    let reg = property_to_u64_vec(node, "reg")?;

    if reg.len() != 2 {
        bail!("memory node {} does not have 2 u64s", node.name);
    }

    let base = reg[0];
    let len = reg[1];
    let numa_node_id = try_find_property(node, "numa-node-id")
        .context(format!("{} missing numa-node-id", node.name))?
        .read_u32(0)
        .map_err(err_to_owned)
        .context("unable to read numa-node-id")?;

    Ok(MemoryRangeWithNode {
        range: MemoryRange::try_new(base..base + len).context("invalid memory range")?,
        vnode: numa_node_id,
    })
}

/// Parse GIC config
fn parse_gic(node: &Node<'_>) -> anyhow::Result<GicInfo> {
    let reg = property_to_u64_vec(node, "reg")?;

    if reg.len() != 4 {
        bail!("gic node {} does not have 4 u64s", node.name);
    }

    Ok(GicInfo {
        gic_distributor_base: reg[0],
        gic_redistributors_base: reg[2],
    })
}

fn parse_pmu(node: &Node<'_>) -> anyhow::Result<u32> {
    let interrupts =
        try_find_property(node, "interrupts").context("pmu node missing interrupts")?;
    let interrupts_typ = interrupts
        .read_u32(0)
        .map_err(err_to_owned)
        .context("missing pmu interrupts typ")?;
    let interrupts_ppi_index = interrupts
        .read_u32(1)
        .map_err(err_to_owned)
        .context("missing pmu interrupts index")?;

    // The interrupt is expected to be a PPI.
    const GIC_PPI: u32 = 1;
    if interrupts_typ != GIC_PPI {
        bail!("pmu node has unexpected interrupt type {interrupts_typ}");
    }

    // The index is the index off of the PPI base of 16. PPIs only exist from 16
    // to 31, so the index should be < 16.
    if interrupts_ppi_index >= 16 {
        bail!("pmu node has unexpected interrupt index {interrupts_ppi_index}");
    }

    const PPI_BASE: u32 = 16;
    Ok(PPI_BASE + interrupts_ppi_index)
}

impl ParsedBootDtInfo {
    /// Read parameters passed via device tree by openhcl_boot, at
    /// /sys/firmware/fdt.
    ///
    /// The device tree is expected to be well formed from the bootloader, so
    /// any errors here are not expected.
    pub fn new() -> anyhow::Result<Self> {
        let raw = fs_err::read("/sys/firmware/fdt").context("reading fdt")?;
        Self::new_from_raw(&raw)
    }

    fn new_from_raw(raw: &[u8]) -> anyhow::Result<Self> {
        let mut cpus = Vec::new();
        let mut vtl0_mmio = Vec::new();
        let mut config_ranges = Vec::new();
        let mut vtl2_memory = Vec::new();
        let mut gic = None;
        let mut pmu_gsiv = None;
        let mut partition_memory_map = Vec::new();
        let mut accepted_ranges = Vec::new();
        let mut vtl0_alias_map = None;
        let mut memory_allocation_mode = MemoryAllocationMode::Host;
        let mut isolation = IsolationType::None;
        let mut vtl2_reserved_range = MemoryRange::EMPTY;
        let mut private_pool_ranges = Vec::new();
        let mut vtl2_persisted_header = MemoryRange::EMPTY;
        let mut vtl2_persisted_protobuf_region = MemoryRange::EMPTY;

        let parser = Parser::new(raw)
            .map_err(err_to_owned)
            .context("failed to create fdt parser")?;

        for child in parser
            .root()
            .map_err(err_to_owned)
            .context("root invalid")?
            .children()
        {
            let child = child.map_err(err_to_owned).context("child invalid")?;

            match child.name {
                "cpus" => {
                    cpus = parse_cpus(&child)?;
                }

                "openhcl" => {
                    let OpenhclInfo {
                        vtl0_mmio: n_vtl0_mmio,
                        config_ranges: n_config_ranges,
                        partition_memory_map: n_partition_memory_map,
                        vtl2_reserved_range: n_vtl2_reserved_range,
                        accepted_memory: n_accepted_memory,
                        vtl0_alias_map: n_vtl0_alias_map,
                        memory_allocation_mode: n_memory_allocation_mode,
                        isolation: n_isolation,
                        private_pool_ranges: n_private_pool_ranges,
                        vtl2_persisted_header: n_vtl2_persisted_header,
                        vtl2_persisted_protobuf_region: n_vtl2_persisted_protobuf_region,
                    } = parse_openhcl(&child)?;
                    vtl0_mmio = n_vtl0_mmio;
                    config_ranges = n_config_ranges;
                    partition_memory_map = n_partition_memory_map;
                    accepted_ranges = n_accepted_memory;
                    vtl0_alias_map = n_vtl0_alias_map;
                    memory_allocation_mode = n_memory_allocation_mode;
                    isolation = n_isolation;
                    vtl2_reserved_range = n_vtl2_reserved_range;
                    private_pool_ranges = n_private_pool_ranges;
                    vtl2_persisted_header = n_vtl2_persisted_header;
                    vtl2_persisted_protobuf_region = n_vtl2_persisted_protobuf_region;
                }

                _ if child.name.starts_with("memory@") => {
                    vtl2_memory.push(parse_memory(&child)?);
                }

                _ if child.name.starts_with("intc@") => {
                    // TODO: make sure we are on aarch64
                    gic = Some(parse_gic(&child)?);
                }

                _ if child.name.starts_with("pmu") => {
                    // TODO: make sure we are on aarch64
                    pmu_gsiv = Some(parse_pmu(&child)?);
                }

                _ => {
                    // Ignore other nodes.
                }
            }
        }

        vtl2_memory.sort_by_key(|r| r.range.start());

        Ok(Self {
            cpus,
            vtl0_mmio,
            config_ranges,
            vtl2_memory,
            partition_memory_map,
            vtl0_alias_map,
            accepted_ranges,
            gic,
            pmu_gsiv,
            memory_allocation_mode,
            isolation,
            vtl2_reserved_range,
            private_pool_ranges,
            vtl2_persisted_header,
            vtl2_persisted_protobuf_region,
        })
    }
}

/// Boot times reported by the bootloader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootTimes {
    /// Kernel start time.
    pub start: Option<u64>,
    /// Kernel end time.
    pub end: Option<u64>,
    /// Sidecar start time.
    pub sidecar_start: Option<u64>,
    /// Sidecar end time.
    pub sidecar_end: Option<u64>,
}

impl BootTimes {
    /// Read the boot times passed via device tree by openhcl_boot, at
    /// /sys/firmware/fdt.
    ///
    /// The device tree is expected to be well formed from the bootloader, so
    /// any errors here are not expected.
    pub fn new() -> anyhow::Result<Self> {
        let raw = fs_err::read("/sys/firmware/fdt").context("reading fdt")?;
        Self::new_from_raw(&raw)
    }

    fn new_from_raw(raw: &[u8]) -> anyhow::Result<Self> {
        let mut start = None;
        let mut end = None;
        let mut sidecar_start = None;
        let mut sidecar_end = None;
        let parser = Parser::new(raw)
            .map_err(err_to_owned)
            .context("failed to create fdt parser")?;

        let root = parser
            .root()
            .map_err(err_to_owned)
            .context("root invalid")?;

        if let Some(prop) = try_find_property(&root, "reftime_boot_start") {
            start = Some(prop.read_u64(0).map_err(err_to_owned)?);
        }

        if let Some(prop) = try_find_property(&root, "reftime_boot_end") {
            end = Some(prop.read_u64(0).map_err(err_to_owned)?);
        }

        if let Some(prop) = try_find_property(&root, "reftime_sidecar_start") {
            sidecar_start = Some(prop.read_u64(0).map_err(err_to_owned)?);
        }

        if let Some(prop) = try_find_property(&root, "reftime_sidecar_end") {
            sidecar_end = Some(prop.read_u64(0).map_err(err_to_owned)?);
        }

        Ok(Self {
            start,
            end,
            sidecar_start,
            sidecar_end,
        })
    }
}

mod inspect_helpers {
    use super::*;

    pub(super) fn memory_internal(ranges: &[AddressRange]) -> impl Inspect + '_ {
        inspect::iter_by_key(ranges.iter().map(|entry| (entry.range(), entry)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt::builder::Builder;

    fn build_dt(info: &ParsedBootDtInfo) -> anyhow::Result<Vec<u8>> {
        let mut buf = vec![0; 4096];

        let mut builder = Builder::new(fdt::builder::BuilderConfig {
            blob_buffer: &mut buf,
            string_table_cap: 1024,
            memory_reservations: &[],
        })?;
        let p_address_cells = builder.add_string("#address-cells")?;
        let p_size_cells = builder.add_string("#size-cells")?;
        let p_reg = builder.add_string("reg")?;
        let p_device_type = builder.add_string("device_type")?;
        let p_compatible = builder.add_string("compatible")?;
        let p_ranges = builder.add_string("ranges")?;
        let p_numa_node_id = builder.add_string("numa-node-id")?;
        let p_igvm_type = builder.add_string(IGVM_DT_IGVM_TYPE_PROPERTY)?;
        let p_openhcl_memory = builder.add_string("openhcl,memory-type")?;
        let p_interrupts = builder.add_string("interrupts")?;

        let mut root_builder = builder
            .start_node("")?
            .add_u32(p_address_cells, 2)?
            .add_u32(p_size_cells, 2)?
            .add_str(p_compatible, "microsoft,openvmm")?;

        let mut cpu_builder = root_builder
            .start_node("cpus")?
            .add_u32(p_address_cells, 1)?
            .add_u32(p_size_cells, 0)?;

        // add cpus
        for (index, cpu) in info.cpus.iter().enumerate() {
            let name = format!("cpu@{}", index + 1);

            cpu_builder = cpu_builder
                .start_node(&name)?
                .add_str(p_device_type, "cpu")?
                .add_u32(p_reg, cpu.reg as u32)?
                .add_u32(p_numa_node_id, cpu.vnode)?
                .end_node()?;
        }

        root_builder = cpu_builder.end_node()?;

        // add memory, in reverse order.
        for memory in info.vtl2_memory.iter().rev() {
            let name = format!("memory@{:x}", memory.range.start());

            root_builder = root_builder
                .start_node(&name)?
                .add_str(p_device_type, "memory")?
                .add_u64_list(p_reg, [memory.range.start(), memory.range.len()])?
                .add_u32(p_numa_node_id, memory.vnode)?
                .end_node()?;
        }

        // GIC
        if let Some(gic) = info.gic {
            let p_interrupt_cells = root_builder.add_string("#interrupt-cells")?;
            let p_redist_regions = root_builder.add_string("#redistributor-regions")?;
            let p_redist_stride = root_builder.add_string("redistributor-stride")?;
            let p_interrupt_controller = root_builder.add_string("interrupt-controller")?;
            let p_phandle = root_builder.add_string("phandle")?;
            let name = format!("intc@{}", gic.gic_distributor_base);
            root_builder = root_builder
                .start_node(name.as_ref())?
                .add_str(p_compatible, "arm,gic-v3")?
                .add_u32(p_redist_regions, 1)?
                .add_u64(p_redist_stride, 0)?
                .add_u64_array(
                    p_reg,
                    &[gic.gic_distributor_base, 0, gic.gic_redistributors_base, 0],
                )?
                .add_u32(p_address_cells, 2)?
                .add_u32(p_size_cells, 2)?
                .add_u32(p_interrupt_cells, 3)?
                .add_null(p_interrupt_controller)?
                .add_u32(p_phandle, 1)?
                .add_null(p_ranges)?
                .end_node()?;
        }

        // PMU
        if let Some(pmu_gsiv) = info.pmu_gsiv {
            assert!((16..32).contains(&pmu_gsiv));
            const GIC_PPI: u32 = 1;
            const IRQ_TYPE_LEVEL_HIGH: u32 = 4;
            root_builder = root_builder
                .start_node("pmu")?
                .add_str(p_compatible, "arm,armv8-pmuv3")?
                .add_u32_array(p_interrupts, &[GIC_PPI, pmu_gsiv - 16, IRQ_TYPE_LEVEL_HIGH])?
                .end_node()?;
        }

        let mut openhcl_builder = root_builder.start_node("openhcl")?;
        let p_isolation_type = openhcl_builder.add_string("isolation-type")?;
        openhcl_builder = openhcl_builder.add_str(
            p_isolation_type,
            match info.isolation {
                IsolationType::None => "none",
                IsolationType::Vbs => "vbs",
                IsolationType::Snp => "snp",
                IsolationType::Tdx => "tdx",
            },
        )?;

        let p_memory_allocation_mode = openhcl_builder.add_string("memory-allocation-mode")?;
        match info.memory_allocation_mode {
            MemoryAllocationMode::Host => {
                openhcl_builder = openhcl_builder.add_str(p_memory_allocation_mode, "host")?;
            }
            MemoryAllocationMode::Vtl2 {
                memory_size,
                mmio_size,
            } => {
                let p_memory_size = openhcl_builder.add_string("memory-size")?;
                let p_mmio_size = openhcl_builder.add_string("mmio-size")?;
                openhcl_builder = openhcl_builder.add_str(p_memory_allocation_mode, "vtl2")?;
                if let Some(memory_size) = memory_size {
                    openhcl_builder = openhcl_builder.add_u64(p_memory_size, memory_size)?;
                }
                if let Some(mmio_size) = mmio_size {
                    openhcl_builder = openhcl_builder.add_u64(p_mmio_size, mmio_size)?;
                }
            }
        }

        if let Some(data) = info.vtl0_alias_map {
            let p_vtl0_alias_map = openhcl_builder.add_string("vtl0-alias-map")?;
            openhcl_builder = openhcl_builder.add_u64(p_vtl0_alias_map, data)?;
        }

        openhcl_builder = openhcl_builder
            .start_node("vmbus-vtl0")?
            .add_u32(p_address_cells, 2)?
            .add_u32(p_size_cells, 2)?
            .add_str(p_compatible, "microsoft,vmbus")?
            .add_u64_list(
                p_ranges,
                info.vtl0_mmio
                    .iter()
                    .flat_map(|r| [r.start(), r.start(), r.len()]),
            )?
            .end_node()?;

        for range in &info.partition_memory_map {
            let name = format!("memory@{:x}", range.range().start());

            let node_builder = openhcl_builder
                .start_node(&name)?
                .add_str(p_device_type, "memory-openhcl")?
                .add_u64_list(p_reg, [range.range().start(), range.range().len()])?
                .add_u32(p_openhcl_memory, range.vtl_usage().0)?;

            openhcl_builder = match range {
                AddressRange::Memory(memory) => {
                    // Add as a memory node, with numa info and igvm type.
                    node_builder
                        .add_u32(p_numa_node_id, memory.range.vnode)?
                        .add_u32(p_igvm_type, memory.igvm_type.0 as u32)?
                }
                AddressRange::Mmio(_) => {
                    // Nothing to do here, mmio already contains the min
                    // required info of range and vtl via vtl_usage.
                    node_builder
                }
            }
            .end_node()?;
        }

        for range in &info.accepted_ranges {
            let name = format!("accepted-memory@{:x}", range.start());

            openhcl_builder = openhcl_builder
                .start_node(&name)?
                .add_str(p_device_type, "memory-openhcl")?
                .add_u64_list(p_reg, [range.start(), range.len()])?
                .end_node()?;
        }

        root_builder = openhcl_builder.end_node()?;

        root_builder.end_node()?.build(info.cpus[0].reg as u32)?;

        Ok(buf)
    }

    #[test]
    fn test_basic() {
        let orig_info = ParsedBootDtInfo {
            cpus: (0..4).map(|i| Cpu { reg: i, vnode: 0 }).collect(),
            vtl2_memory: vec![
                MemoryRangeWithNode {
                    range: MemoryRange::new(0x10000..0x20000),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(0x20000..0x30000),
                    vnode: 1,
                },
            ],
            partition_memory_map: vec![
                AddressRange::Memory(Memory {
                    range: MemoryRangeWithNode {
                        range: MemoryRange::new(0..0x1000),
                        vnode: 0,
                    },
                    vtl_usage: MemoryVtlType::VTL0,
                    igvm_type: MemoryMapEntryType::MEMORY,
                }),
                AddressRange::Mmio(Mmio {
                    range: MemoryRange::new(0x1000..0x2000),
                    vtl: Vtl::Vtl0,
                }),
                AddressRange::Mmio(Mmio {
                    range: MemoryRange::new(0x3000..0x4000),
                    vtl: Vtl::Vtl0,
                }),
                AddressRange::Memory(Memory {
                    range: MemoryRangeWithNode {
                        range: MemoryRange::new(0x10000..0x20000),
                        vnode: 0,
                    },
                    vtl_usage: MemoryVtlType::VTL2_RAM,
                    igvm_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                }),
                AddressRange::Memory(Memory {
                    range: MemoryRangeWithNode {
                        range: MemoryRange::new(0x20000..0x30000),
                        vnode: 1,
                    },
                    vtl_usage: MemoryVtlType::VTL2_CONFIG,
                    igvm_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                }),
                AddressRange::Memory(Memory {
                    range: MemoryRangeWithNode {
                        range: MemoryRange::new(0x30000..0x40000),
                        vnode: 1,
                    },
                    vtl_usage: MemoryVtlType::VTL2_CONFIG,
                    igvm_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                }),
                AddressRange::Memory(Memory {
                    range: MemoryRangeWithNode {
                        range: MemoryRange::new(0x40000..0x50000),
                        vnode: 1,
                    },
                    vtl_usage: MemoryVtlType::VTL2_RESERVED,
                    igvm_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                }),
                AddressRange::Memory(Memory {
                    range: MemoryRangeWithNode {
                        range: MemoryRange::new(0x50000..0x51000),
                        vnode: 0,
                    },
                    vtl_usage: MemoryVtlType::VTL2_PERSISTED_STATE_HEADER,
                    igvm_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                }),
                AddressRange::Memory(Memory {
                    range: MemoryRangeWithNode {
                        range: MemoryRange::new(0x51000..0x52000),
                        vnode: 0,
                    },
                    vtl_usage: MemoryVtlType::VTL2_PERSISTED_STATE_PROTOBUF,
                    igvm_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                }),
                AddressRange::Memory(Memory {
                    range: MemoryRangeWithNode {
                        range: MemoryRange::new(0x60000..0x70000),
                        vnode: 0,
                    },
                    vtl_usage: MemoryVtlType::VTL2_GPA_POOL,
                    igvm_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                }),
                AddressRange::Memory(Memory {
                    range: MemoryRangeWithNode {
                        range: MemoryRange::new(0x1000000..0x2000000),
                        vnode: 0,
                    },
                    vtl_usage: MemoryVtlType::VTL0,
                    igvm_type: MemoryMapEntryType::MEMORY,
                }),
                AddressRange::Mmio(Mmio {
                    range: MemoryRange::new(0x3000000..0x4000000),
                    vtl: Vtl::Vtl2,
                }),
            ],
            vtl0_mmio: vec![
                MemoryRange::new(0x1000..0x2000),
                MemoryRange::new(0x3000..0x4000),
            ],
            config_ranges: vec![
                MemoryRange::new(0x20000..0x30000),
                MemoryRange::new(0x30000..0x40000),
            ],
            vtl0_alias_map: Some(1 << 48),
            gic: Some(GicInfo {
                gic_distributor_base: 0x10000,
                gic_redistributors_base: 0x20000,
            }),
            pmu_gsiv: Some(0x17),
            accepted_ranges: vec![
                MemoryRange::new(0x10000..0x20000),
                MemoryRange::new(0x1000000..0x1500000),
            ],
            memory_allocation_mode: MemoryAllocationMode::Vtl2 {
                memory_size: Some(0x1000),
                mmio_size: Some(0x2000),
            },
            isolation: IsolationType::Vbs,
            vtl2_reserved_range: MemoryRange::new(0x40000..0x50000),
            private_pool_ranges: vec![MemoryRangeWithNode {
                range: MemoryRange::new(0x60000..0x70000),
                vnode: 0,
            }],
            vtl2_persisted_header: MemoryRange::new(0x50000..0x51000),
            vtl2_persisted_protobuf_region: MemoryRange::new(0x51000..0x52000),
        };

        let dt = build_dt(&orig_info).unwrap();
        let parsed = ParsedBootDtInfo::new_from_raw(&dt).unwrap();

        assert_eq!(orig_info, parsed);
    }

    fn build_boottime_dt(boot_times: BootTimes) -> anyhow::Result<Vec<u8>> {
        let mut buf = vec![0; 4096];

        let mut builder = Builder::new(fdt::builder::BuilderConfig {
            blob_buffer: &mut buf,
            string_table_cap: 1024,
            memory_reservations: &[],
        })?;
        let p_address_cells = builder.add_string("#address-cells")?;
        let p_size_cells = builder.add_string("#size-cells")?;
        let p_reftime_boot_start = builder.add_string("reftime_boot_start")?;
        let p_reftime_boot_end = builder.add_string("reftime_boot_end")?;
        let p_reftime_sidecar_start = builder.add_string("reftime_sidecar_start")?;
        let p_reftime_sidecar_end = builder.add_string("reftime_sidecar_end")?;

        let mut root_builder = builder
            .start_node("")?
            .add_u32(p_address_cells, 2)?
            .add_u32(p_size_cells, 2)?;

        if let Some(start) = boot_times.start {
            root_builder = root_builder.add_u64(p_reftime_boot_start, start)?;
        }

        if let Some(end) = boot_times.end {
            root_builder = root_builder.add_u64(p_reftime_boot_end, end)?;
        }

        if let Some(start) = boot_times.sidecar_start {
            root_builder = root_builder.add_u64(p_reftime_sidecar_start, start)?;
        }

        if let Some(end) = boot_times.sidecar_end {
            root_builder = root_builder.add_u64(p_reftime_sidecar_end, end)?;
        }

        root_builder.end_node()?.build(0)?;

        Ok(buf)
    }

    #[test]
    fn test_basic_boottime() {
        let orig_info = BootTimes {
            start: Some(0x1000),
            end: Some(0x2000),
            sidecar_start: Some(0x3000),
            sidecar_end: Some(0x4000),
        };

        let dt = build_boottime_dt(orig_info).unwrap();
        let parsed = BootTimes::new_from_raw(&dt).unwrap();

        assert_eq!(orig_info, parsed);

        // test no boot times.
        let orig_info = BootTimes {
            start: None,
            end: None,
            sidecar_start: None,
            sidecar_end: None,
        };

        let dt = build_boottime_dt(orig_info).unwrap();
        let parsed = BootTimes::new_from_raw(&dt).unwrap();

        assert_eq!(orig_info, parsed);
    }
}
