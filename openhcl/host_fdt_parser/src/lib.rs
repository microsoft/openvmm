// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common parsing code for parsing the device tree provided by the host.
//! Note that is is not a generic device tree parser, but parses the device tree
//! for devices and concepts specific to underhill.
//!
//! Notably, we search for IGVM specific extensions to nodes, defined here:
//! [`igvm_defs::dt`].

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use arrayvec::ArrayString;
use arrayvec::ArrayVec;
use core::fmt::Display;
use core::fmt::Write;
use core::mem::size_of;
use hvdef::HV_PAGE_SIZE;
use igvm_defs::MemoryMapEntryType;
#[cfg(feature = "inspect")]
use inspect::Inspect;
use memory_range::MemoryRange;

/// Information about VMBUS.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
pub struct VmbusInfo {
    /// Parsed sorted mmio ranges from the device tree.
    #[cfg_attr(feature = "inspect", inspect(with = "inspect_helpers::mmio_internal"))]
    pub mmio: ArrayVec<MemoryRange, 2>,
    /// Connection ID for the vmbus root device.
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub connection_id: u32,
}

/// Information about the GIC.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
pub struct GicInfo {
    /// GIC distributor base
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub gic_distributor_base: u64,
    /// GIC distributor size
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub gic_distributor_size: u64,
    /// GIC redistributors base
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub gic_redistributors_base: u64,
    /// GIC redistributor block size
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub gic_redistributors_size: u64,
    /// GIC redistributor size
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub gic_redistributor_stride: u64,
}

/// Errors returned by parsing.
#[derive(Debug)]
pub struct Error<'a>(ErrorKind<'a>);

impl Display for Error<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("Parsing failed due to: {}", self.0))
    }
}

impl core::error::Error for Error<'_> {}

#[derive(Debug)]
enum ErrorKind<'a> {
    Dt(fdt::parser::Error<'a>),
    Node {
        parent_name: &'a str,
        error: fdt::parser::Error<'a>,
    },
    PropMissing {
        node_name: &'a str,
        prop_name: &'static str,
    },
    Prop(fdt::parser::Error<'a>),
    TooManyCpus,
    MemoryRegUnaligned {
        node_name: &'a str,
        base: u64,
        len: u64,
    },
    MemoryRegOverlap {
        lower: MemoryEntry,
        upper: MemoryEntry,
    },
    TooManyMemoryEntries,
    PropInvalidU32 {
        node_name: &'a str,
        prop_name: &'a str,
        expected: u32,
        actual: u32,
    },
    PropInvalidStr {
        node_name: &'a str,
        prop_name: &'a str,
        expected: &'a str,
        actual: &'a str,
    },
    UnexpectedVmbusVtl {
        node_name: &'a str,
        vtl: u32,
    },
    MultipleVmbusNode {
        node_name: &'a str,
    },
    VmbusRangesChildParent {
        node_name: &'a str,
        child_base: u64,
        parent_base: u64,
    },
    VmbusRangesNotAligned {
        node_name: &'a str,
        base: u64,
        len: u64,
    },
    TooManyVmbusMmioRanges {
        node_name: &'a str,
        ranges: usize,
    },
    VmbusMmioOverlapsRam {
        mmio: MemoryRange,
        ram: MemoryEntry,
    },
    VmbusMmioOverlapsVmbusMmio {
        mmio_a: MemoryRange,
        mmio_b: MemoryRange,
    },
    CmdlineSize,
    UnexpectedMemoryAllocationMode {
        mode: &'a str,
    },
}

impl Display for ErrorKind<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ErrorKind::Dt(e) => f.write_fmt(format_args!("invalid device tree: {}", e)),
            ErrorKind::Node { parent_name, error } => {
                f.write_fmt(format_args!("invalid device tree node with parent {parent_name}: {error}"))
            }
            ErrorKind::PropMissing {
                node_name,
                prop_name,
            } => f.write_fmt(format_args!(
                "{node_name} did not have the following required property {prop_name}",
            )),
            ErrorKind::Prop(e) => f.write_fmt(format_args!("reading node property failed: {e}")),
            ErrorKind::TooManyCpus => {
                f.write_str("device tree contained more enabled CPUs than can be parsed")
            }
            ErrorKind::MemoryRegUnaligned {
                node_name,
                base,
                len,
            } => f.write_fmt(format_args!(
                "memory node {node_name} contains 4K unaligned base {base} or len {len}"
            )),
            ErrorKind::MemoryRegOverlap { lower, upper,  } => {
                f.write_fmt(format_args!("ram at {}..{} of type {:?} overlaps ram at {}..{} of type {:?}", lower.range.start(), lower.range.end(), lower.mem_type, upper.range.start(), upper.range.end(), upper.mem_type))
            }
            ErrorKind::TooManyMemoryEntries => {
                f.write_str("device tree contained more memory ranges than can be parsed")
            }
            ErrorKind::PropInvalidU32 { node_name, prop_name, expected, actual } => f.write_fmt(format_args!("{node_name} had an invalid u32 value for {prop_name}: expected {expected}, actual {actual}")),
            ErrorKind::PropInvalidStr { node_name, prop_name, expected, actual } => f.write_fmt(format_args!("{node_name} had an invalid str value for {prop_name}: expected {expected}, actual {actual}")),
            ErrorKind::UnexpectedVmbusVtl { node_name, vtl } => f.write_fmt(format_args!("{node_name} has an unexpected vtl {vtl}")),
            ErrorKind::MultipleVmbusNode { node_name } => f.write_fmt(format_args!("{node_name} specifies a duplicate vmbus node")),
            ErrorKind::VmbusRangesChildParent { node_name, child_base, parent_base } => f.write_fmt(format_args!("vmbus {node_name} ranges child base {child_base} does not match parent {parent_base}")),
            ErrorKind::VmbusRangesNotAligned { node_name, base, len } => f.write_fmt(format_args!("vmbus {node_name} base {base} or len {len} not aligned to 4K")),
            ErrorKind::TooManyVmbusMmioRanges { node_name, ranges } => f.write_fmt(format_args!("vmbus {node_name} has more than 2 mmio ranges {ranges}")),
            ErrorKind::VmbusMmioOverlapsRam { mmio, ram } => {
                f.write_fmt(format_args!("vmbus mmio at {}..{} overlaps ram at {}..{}", mmio.start(), mmio.end(), ram.range.start(), ram.range.end()))
            }
            ErrorKind::VmbusMmioOverlapsVmbusMmio { mmio_a, mmio_b } => {
                f.write_fmt(format_args!("vmbus mmio at {}..{} overlaps vmbus mmio at {}..{}", mmio_a.start(), mmio_a.end(), mmio_b.start(), mmio_b.end()))
            }
            ErrorKind::CmdlineSize => f.write_str("commandline too small to parse /chosen bootargs"),
            ErrorKind::UnexpectedMemoryAllocationMode { mode } => f.write_fmt(format_args!("unexpected memory allocation mode: {}", mode)),
        }
    }
}

const COM3_REG_BASE: u64 = 0x3E8;

/// Struct containing parsed device tree information.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
pub struct ParsedDeviceTree<
    const MAX_MEMORY_ENTRIES: usize,
    const MAX_CPU_ENTRIES: usize,
    const MAX_COMMAND_LINE_SIZE: usize,
    const MAX_ENTROPY_SIZE: usize,
> {
    /// Total size of the parsed device tree, in bytes.
    pub device_tree_size: usize,
    /// Parsed sorted memory ranges from the device tree.
    #[cfg_attr(
        feature = "inspect",
        inspect(with = "inspect_helpers::memory_internal")
    )]
    pub memory: ArrayVec<MemoryEntry, MAX_MEMORY_ENTRIES>,
    /// Boot cpu physical id. On X64, this is the APIC id of the BSP.
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub boot_cpuid_phys: u32,
    /// Information for enabled cpus.
    #[cfg_attr(feature = "inspect", inspect(iter_by_index))]
    pub cpus: ArrayVec<CpuEntry, MAX_CPU_ENTRIES>,
    /// VMBUS info for VTL0.
    pub vmbus_vtl0: Option<VmbusInfo>,
    /// VMBUS info for VTL2.
    pub vmbus_vtl2: Option<VmbusInfo>,
    /// Command line contained in the `/chosen` node.
    /// FUTURE: return more information from the chosen node.
    #[cfg_attr(feature = "inspect", inspect(display))]
    pub command_line: ArrayString<MAX_COMMAND_LINE_SIZE>,
    /// Is a com3 device present
    pub com3_serial: bool,
    /// GIC information
    pub gic: Option<GicInfo>,
    /// The vtl2 memory allocation mode OpenHCL should use for memory.
    pub memory_allocation_mode: MemoryAllocationMode,
    /// Entropy from the host to be used by the OpenHCL kernel
    #[cfg_attr(feature = "inspect", inspect(with = "Option::is_some"))]
    pub entropy: Option<ArrayVec<u8, MAX_ENTROPY_SIZE>>,
    /// The number of pages the host has provided as a hint for device dma.
    ///
    /// This is used to allocate a persistent VTL2 pool on non-isolated guests,
    /// to allow devices to stay alive during a servicing operation.
    pub device_dma_page_count: Option<u64>,
    /// Indicates that Host does support NVMe keep-alive.
    pub nvme_keepalive: bool,
    /// The physical address of the VTL0 alias mapping, if one is configured.
    pub vtl0_alias_map: Option<u64>,
}

/// The memory allocation mode provided by the host. This determines how OpenHCL
/// will allocate memory for itself from the partition memory map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
#[cfg_attr(feature = "inspect", inspect(external_tag))]
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
        memory_size: Option<u64>,
        /// The number of bytes VTL2 should allocate for mmio for itself.
        /// Encoded as `openhcl/mmio-size` in device tree.
        mmio_size: Option<u64>,
    },
}

/// Struct containing parsed memory information.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
pub struct MemoryEntry {
    /// The range of addresses covered by this entry.
    pub range: MemoryRange,
    /// The type of memory of this entry.
    #[cfg_attr(
        feature = "inspect",
        inspect(with = "inspect_helpers::inspect_memory_map_entry_type")
    )]
    pub mem_type: MemoryMapEntryType,
    /// The numa node id of this entry.
    pub vnode: u32,
}

/// Struct containing parsed CPU information.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
pub struct CpuEntry {
    /// Architecture specific "reg" value for this CPU.
    /// For x64, this is the APIC ID.
    /// For ARM v8 64-bit, this should match the MPIDR_EL1 register affinity bits.
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub reg: u64,
    /// Numa node id for this CPU.
    pub vnode: u32,
}

impl<
        'a,
        'b,
        const MAX_MEMORY_ENTRIES: usize,
        const MAX_CPU_ENTRIES: usize,
        const MAX_COMMAND_LINE_SIZE: usize,
        const MAX_ENTROPY_SIZE: usize,
    >
    ParsedDeviceTree<MAX_MEMORY_ENTRIES, MAX_CPU_ENTRIES, MAX_COMMAND_LINE_SIZE, MAX_ENTROPY_SIZE>
{
    /// Create an empty parsed device tree structure. This is used to construct
    /// a valid instance to pass into [`Self::parse`].
    pub const fn new() -> Self {
        Self {
            device_tree_size: 0,
            memory: ArrayVec::new_const(),
            boot_cpuid_phys: 0,
            cpus: ArrayVec::new_const(),
            vmbus_vtl0: None,
            vmbus_vtl2: None,
            command_line: ArrayString::new_const(),
            com3_serial: false,
            gic: None,
            memory_allocation_mode: MemoryAllocationMode::Host,
            entropy: None,
            device_dma_page_count: None,
            nvme_keepalive: false,
            vtl0_alias_map: None,
        }
    }

    /// The number of enabled cpus.
    pub fn cpu_count(&self) -> usize {
        self.cpus.len()
    }

    /// Parse the given device tree.
    pub fn parse(dt: &'a [u8], storage: &'b mut Self) -> Result<&'b Self, Error<'a>> {
        Self::parse_inner(dt, storage).map_err(Error)
    }

    fn parse_inner(dt: &'a [u8], storage: &'b mut Self) -> Result<&'b Self, ErrorKind<'a>> {
        let parser = fdt::parser::Parser::new(dt).map_err(ErrorKind::Dt)?;
        let root = match parser.root() {
            Ok(v) => v,
            Err(e) => {
                return Err(ErrorKind::Node {
                    parent_name: "",
                    error: e,
                })
            }
        };

        // Insert a memory entry into sorted parsed memory entries.
        //
        // TODO: This could be replaced with appending at the end with sort call
        // after all entries are parsed once sort is stabilized in core.
        let insert_memory_entry = |memory: &mut ArrayVec<MemoryEntry, MAX_MEMORY_ENTRIES>,
                                   entry: MemoryEntry|
         -> Result<(), ErrorKind<'a>> {
            let insert_index = match memory.binary_search_by_key(&entry.range, |k| k.range) {
                Ok(index) => {
                    return Err(ErrorKind::MemoryRegOverlap {
                        lower: memory[index],
                        upper: entry,
                    })
                }
                Err(index) => index,
            };

            memory
                .try_insert(insert_index, entry)
                .map_err(|_| ErrorKind::TooManyMemoryEntries)
        };

        for child in root.children() {
            let child = child.map_err(|error| ErrorKind::Node {
                parent_name: root.name,
                error,
            })?;

            match child.name {
                "cpus" => {
                    let address_cells = child
                        .find_property("#address-cells")
                        .map_err(ErrorKind::Prop)?
                        .ok_or(ErrorKind::PropMissing {
                            node_name: child.name,
                            prop_name: "#address-cells",
                        })?
                        .read_u32(0)
                        .map_err(ErrorKind::Prop)?;

                    // On ARM v8 64-bit systems, up to 2 address-cells values
                    // can be provided.
                    if address_cells > 2 {
                        return Err(ErrorKind::PropInvalidU32 {
                            node_name: child.name,
                            prop_name: "#address-cells",
                            expected: 2,
                            actual: address_cells,
                        });
                    }

                    for cpu in child.children() {
                        let cpu = cpu.map_err(|error| ErrorKind::Node {
                            parent_name: child.name,
                            error,
                        })?;

                        if cpu
                            .find_property("status")
                            .map_err(ErrorKind::Prop)?
                            .ok_or(ErrorKind::PropMissing {
                                node_name: cpu.name,
                                prop_name: "status",
                            })?
                            .read_str()
                            .map_err(ErrorKind::Prop)?
                            != "okay"
                        {
                            continue;
                        }

                        // NOTE: For x86, Underhill will need to query the hypervisor for
                        // the vp_index to apic_id mapping. There's no
                        // correlation in the device tree about this at all.
                        let reg_property = cpu
                            .find_property("reg")
                            .map_err(ErrorKind::Prop)?
                            .ok_or(ErrorKind::PropMissing {
                                node_name: cpu.name,
                                prop_name: "reg",
                            })?;

                        let reg = if address_cells == 1 {
                            reg_property.read_u32(0).map_err(ErrorKind::Prop)? as u64
                        } else {
                            reg_property.read_u64(0).map_err(ErrorKind::Prop)?
                        };

                        let vnode = cpu
                            .find_property("numa-node-id")
                            .map_err(ErrorKind::Prop)?
                            .ok_or(ErrorKind::PropMissing {
                                node_name: cpu.name,
                                prop_name: "numa-node-id",
                            })?
                            .read_u32(0)
                            .map_err(ErrorKind::Prop)?;

                        storage
                            .cpus
                            .try_push(CpuEntry { reg, vnode })
                            .map_err(|_| ErrorKind::TooManyCpus)?;
                    }
                }
                "openhcl" => {
                    let memory_allocation_mode = child
                        .find_property("memory-allocation-mode")
                        .map_err(ErrorKind::Prop)?
                        .ok_or(ErrorKind::PropMissing {
                            node_name: child.name,
                            prop_name: "memory-allocation-mode",
                        })?;

                    match memory_allocation_mode.read_str().map_err(ErrorKind::Prop)? {
                        "host" => {
                            storage.memory_allocation_mode = MemoryAllocationMode::Host;
                        }
                        "vtl2" => {
                            let memory_size = child
                                .find_property("memory-size")
                                .map_err(ErrorKind::Prop)?
                                .map(|p| p.read_u64(0))
                                .transpose()
                                .map_err(ErrorKind::Prop)?;

                            let mmio_size = child
                                .find_property("mmio-size")
                                .map_err(ErrorKind::Prop)?
                                .map(|p| p.read_u64(0))
                                .transpose()
                                .map_err(ErrorKind::Prop)?;

                            storage.memory_allocation_mode = MemoryAllocationMode::Vtl2 {
                                memory_size,
                                mmio_size,
                            };
                        }
                        mode => {
                            return Err(ErrorKind::UnexpectedMemoryAllocationMode { mode });
                        }
                    }

                    storage.vtl0_alias_map = child
                        .find_property("vtl0-alias-map")
                        .map_err(ErrorKind::Prop)?
                        .map(|p| p.read_u64(0))
                        .transpose()
                        .map_err(ErrorKind::Prop)?;

                    for openhcl_child in child.children() {
                        let openhcl_child = openhcl_child.map_err(|error| ErrorKind::Node {
                            parent_name: root.name,
                            error,
                        })?;

                        #[allow(clippy::single_match)]
                        match openhcl_child.name {
                            "entropy" => {
                                let host_entropy = openhcl_child
                                    .find_property("reg")
                                    .map_err(ErrorKind::Prop)?
                                    .ok_or(ErrorKind::PropMissing {
                                        node_name: openhcl_child.name,
                                        prop_name: "reg",
                                    })?
                                    .data;

                                if host_entropy.len() > MAX_ENTROPY_SIZE {
                                    #[cfg(feature = "tracing")]
                                    tracing::warn!(
                                        entropy_len = host_entropy.len(),
                                        "Truncating host-provided entropy",
                                    );
                                }
                                let use_entropy_bytes =
                                    core::cmp::min(host_entropy.len(), MAX_ENTROPY_SIZE);
                                let entropy =
                                    ArrayVec::try_from(&host_entropy[..use_entropy_bytes]).unwrap();

                                storage.entropy = Some(entropy);
                            }
                            // These parameters may not be present so it is not an error if they are missing.
                            "keep-alive" => {
                                storage.nvme_keepalive = openhcl_child
                                    .find_property("device-types")
                                    .ok()
                                    .flatten()
                                    .and_then(|p| p.read_str().ok())
                                    == Some("nvme");
                            }
                            "device-dma" => {
                                // DMA reserved page count hint.
                                storage.device_dma_page_count = openhcl_child
                                    .find_property("total-pages")
                                    .ok()
                                    .flatten()
                                    .and_then(|p| p.read_u64(0).ok());
                            }
                            _ => {
                                #[cfg(feature = "tracing")]
                                tracing::warn!(?openhcl_child.name, "Unrecognized OpenHCL child node");
                            }
                        }
                    }
                }

                _ if child.name.starts_with("memory@") => {
                    let igvm_type = if let Some(igvm_type) = child
                        .find_property(igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY)
                        .map_err(ErrorKind::Prop)?
                    {
                        let typ = igvm_type.read_u32(0).map_err(ErrorKind::Prop)?;
                        MemoryMapEntryType(typ as u16)
                    } else {
                        MemoryMapEntryType::MEMORY
                    };

                    let reg = child.find_property("reg").map_err(ErrorKind::Prop)?.ok_or(
                        ErrorKind::PropMissing {
                            node_name: child.name,
                            prop_name: "reg",
                        },
                    )?;

                    let vnode = child
                        .find_property("numa-node-id")
                        .map_err(ErrorKind::Prop)?
                        .ok_or(ErrorKind::PropMissing {
                            node_name: child.name,
                            prop_name: "numa-node-id",
                        })?
                        .read_u32(0)
                        .map_err(ErrorKind::Prop)?;

                    let len = reg.data.len();
                    let reg_tuple_size = size_of::<u64>() * 2;
                    let number_of_ranges = len / reg_tuple_size;

                    for i in 0..number_of_ranges {
                        let base = reg.read_u64(i * 2).map_err(ErrorKind::Prop)?;
                        let len = reg.read_u64(i * 2 + 1).map_err(ErrorKind::Prop)?;

                        if base % HV_PAGE_SIZE != 0 || len % HV_PAGE_SIZE != 0 {
                            return Err(ErrorKind::MemoryRegUnaligned {
                                node_name: child.name,
                                base,
                                len,
                            });
                        }

                        insert_memory_entry(
                            &mut storage.memory,
                            MemoryEntry {
                                range: MemoryRange::try_new(base..(base + len))
                                    .expect("valid range"),
                                mem_type: igvm_type,
                                vnode,
                            },
                        )?;
                    }
                }
                "chosen" => {
                    let cmdline = child
                        .find_property("bootargs")
                        .map_err(ErrorKind::Prop)?
                        .map(|prop| prop.read_str().map_err(ErrorKind::Prop))
                        .transpose()?
                        .unwrap_or("");

                    write!(storage.command_line, "{}", cmdline)
                        .map_err(|_| ErrorKind::CmdlineSize)?;
                }
                _ if child.name.starts_with("intc@") => {
                    validate_property_str(&child, "compatible", "arm,gic-v3")?;
                    validate_property_u32(&child, "#redistributor-regions", 1, 0)?;
                    validate_property_u32(&child, "#address-cells", 2, 0)?;
                    validate_property_u32(&child, "#size-cells", 2, 0)?;
                    validate_property_u32(&child, "#interrupt-cells", 3, 0)?;

                    let gic_redistributor_stride = child
                        .find_property("redistributor-stride")
                        .map_err(ErrorKind::Prop)?
                        .ok_or(ErrorKind::PropMissing {
                            node_name: child.name,
                            prop_name: "redistributor-stride",
                        })?
                        .read_u64(0)
                        .map_err(ErrorKind::Prop)?;

                    let gic_reg_property = child
                        .find_property("reg")
                        .map_err(ErrorKind::Prop)?
                        .ok_or(ErrorKind::PropMissing {
                            node_name: child.name,
                            prop_name: "reg",
                        })?;
                    let gic_distributor_base =
                        gic_reg_property.read_u64(0).map_err(ErrorKind::Prop)?;
                    let gic_distributor_size =
                        gic_reg_property.read_u64(1).map_err(ErrorKind::Prop)?;
                    let gic_redistributors_base =
                        gic_reg_property.read_u64(2).map_err(ErrorKind::Prop)?;
                    let gic_redistributors_size =
                        gic_reg_property.read_u64(3).map_err(ErrorKind::Prop)?;

                    storage.gic = Some(GicInfo {
                        gic_distributor_base,
                        gic_distributor_size,
                        gic_redistributors_base,
                        gic_redistributors_size,
                        gic_redistributor_stride,
                    })
                }
                _ => {
                    parse_compatible(
                        &child,
                        &mut storage.vmbus_vtl0,
                        &mut storage.vmbus_vtl2,
                        &mut storage.com3_serial,
                    )?;
                }
            }
        }

        // Validate memory entries do not overlap.
        for (prev, next) in storage.memory.iter().zip(storage.memory.iter().skip(1)) {
            if prev.range.overlaps(&next.range) {
                return Err(ErrorKind::MemoryRegOverlap {
                    lower: *prev,
                    upper: *next,
                });
            }
        }

        // Validate no mmio ranges overlap each other, or memory.
        let vmbus_vtl0_mmio = storage
            .vmbus_vtl0
            .as_ref()
            .map(|info| info.mmio.as_slice())
            .unwrap_or(&[]);

        let vmbus_vtl2_mmio = storage
            .vmbus_vtl2
            .as_ref()
            .map(|info| info.mmio.as_slice())
            .unwrap_or(&[]);

        for ram in storage.memory.iter() {
            for mmio in vmbus_vtl0_mmio {
                if mmio.overlaps(&ram.range) {
                    return Err(ErrorKind::VmbusMmioOverlapsRam {
                        mmio: *mmio,
                        ram: *ram,
                    });
                }
            }

            for mmio in vmbus_vtl2_mmio {
                if mmio.overlaps(&ram.range) {
                    return Err(ErrorKind::VmbusMmioOverlapsRam {
                        mmio: *mmio,
                        ram: *ram,
                    });
                }
            }
        }

        for vtl0_mmio in vmbus_vtl0_mmio {
            for vtl2_mmio in vmbus_vtl2_mmio {
                if vtl0_mmio.overlaps(vtl2_mmio) {
                    return Err(ErrorKind::VmbusMmioOverlapsVmbusMmio {
                        mmio_a: *vtl0_mmio,
                        mmio_b: *vtl2_mmio,
                    });
                }
            }
        }

        // Set remaining fields that were not already filled out.
        let Self {
            device_tree_size,
            memory: _,
            boot_cpuid_phys,
            cpus: _,
            vmbus_vtl0: _,
            vmbus_vtl2: _,
            command_line: _,
            com3_serial: _,
            gic: _,
            memory_allocation_mode: _,
            entropy: _,
            device_dma_page_count: _,
            nvme_keepalive: _,
            vtl0_alias_map: _,
        } = storage;

        *device_tree_size = parser.total_size;
        *boot_cpuid_phys = parser.boot_cpuid_phys;

        Ok(storage)
    }
}

fn parse_compatible<'a>(
    node: &fdt::parser::Node<'a>,
    vmbus_vtl0: &mut Option<VmbusInfo>,
    vmbus_vtl2: &mut Option<VmbusInfo>,
    com3_serial: &mut bool,
) -> Result<(), ErrorKind<'a>> {
    let compatible = node
        .find_property("compatible")
        .map_err(ErrorKind::Prop)?
        .map(|prop| prop.read_str().map_err(ErrorKind::Prop))
        .transpose()?
        .unwrap_or("");

    if compatible == "simple-bus" {
        parse_simple_bus(node, vmbus_vtl0, vmbus_vtl2)?;
    } else if compatible == "x86-pio-bus" {
        parse_io_bus(node, com3_serial)?;
    } else {
        #[cfg(feature = "tracing")]
        tracing::warn!(?compatible, ?node.name,
            "Unrecognized compatible field",
        );
    }

    Ok(())
}

fn parse_vmbus<'a>(node: &fdt::parser::Node<'a>) -> Result<VmbusInfo, ErrorKind<'a>> {
    // Validate address cells and size cells are 2
    let address_cells = node
        .find_property("#address-cells")
        .map_err(ErrorKind::Prop)?
        .ok_or(ErrorKind::PropMissing {
            node_name: node.name,
            prop_name: "#address-cells",
        })?
        .read_u32(0)
        .map_err(ErrorKind::Prop)?;

    if address_cells != 2 {
        return Err(ErrorKind::PropInvalidU32 {
            node_name: node.name,
            prop_name: "#address-cells",
            expected: 2,
            actual: address_cells,
        });
    }

    let size_cells = node
        .find_property("#size-cells")
        .map_err(ErrorKind::Prop)?
        .ok_or(ErrorKind::PropMissing {
            node_name: node.name,
            prop_name: "#size-cells",
        })?
        .read_u32(0)
        .map_err(ErrorKind::Prop)?;

    if size_cells != 2 {
        return Err(ErrorKind::PropInvalidU32 {
            node_name: node.name,
            prop_name: "#size-cells",
            expected: 2,
            actual: size_cells,
        });
    }

    let mmio: ArrayVec<MemoryRange, 2> =
        match node.find_property("ranges").map_err(ErrorKind::Prop)? {
            Some(ranges) => {
                // Determine how many mmio ranges this describes. Valid numbers are
                // 0, 1 or 2.
                let ranges_tuple_size = size_of::<u64>() * 3;
                let number_of_ranges = ranges.data.len() / ranges_tuple_size;
                let mut mmio = ArrayVec::new();

                if number_of_ranges > 2 {
                    return Err(ErrorKind::TooManyVmbusMmioRanges {
                        node_name: node.name,
                        ranges: number_of_ranges,
                    });
                }

                for i in 0..number_of_ranges {
                    let child_base = ranges.read_u64(i * 3).map_err(ErrorKind::Prop)?;
                    let parent_base = ranges.read_u64(i * 3 + 1).map_err(ErrorKind::Prop)?;
                    let len = ranges.read_u64(i * 3 + 2).map_err(ErrorKind::Prop)?;

                    if child_base != parent_base {
                        return Err(ErrorKind::VmbusRangesChildParent {
                            node_name: node.name,
                            child_base,
                            parent_base,
                        });
                    }

                    if child_base % HV_PAGE_SIZE != 0 || len % HV_PAGE_SIZE != 0 {
                        return Err(ErrorKind::VmbusRangesNotAligned {
                            node_name: node.name,
                            base: child_base,
                            len,
                        });
                    }

                    mmio.push(
                        MemoryRange::try_new(child_base..(child_base + len)).expect("valid range"),
                    );
                }

                // The DT ranges field might not have been sorted. Swap them if the
                // low gap was described 2nd.
                if number_of_ranges > 1 && mmio[0].start() > mmio[1].start() {
                    mmio.swap(0, 1);
                }

                if number_of_ranges > 1 && mmio[0].overlaps(&mmio[1]) {
                    return Err(ErrorKind::VmbusMmioOverlapsVmbusMmio {
                        mmio_a: mmio[0],
                        mmio_b: mmio[1],
                    });
                }

                mmio
            }
            None => {
                // No mmio is acceptable.
                ArrayVec::new()
            }
        };

    let connection_id = node
        .find_property("microsoft,message-connection-id")
        .map_err(ErrorKind::Prop)?
        .ok_or(ErrorKind::PropMissing {
            node_name: node.name,
            prop_name: "microsoft,message-connection-id",
        })?
        .read_u32(0)
        .map_err(ErrorKind::Prop)?;

    Ok(VmbusInfo {
        mmio,
        connection_id,
    })
}

fn parse_simple_bus<'a>(
    node: &fdt::parser::Node<'a>,
    vmbus_vtl0: &mut Option<VmbusInfo>,
    vmbus_vtl2: &mut Option<VmbusInfo>,
) -> Result<(), ErrorKind<'a>> {
    // Vmbus must be under simple-bus node with empty ranges.
    if !node
        .find_property("ranges")
        .map_err(ErrorKind::Prop)?
        .ok_or(ErrorKind::PropMissing {
            node_name: node.name,
            prop_name: "ranges",
        })?
        .data
        .is_empty()
    {
        return Ok(());
    }

    for child in node.children() {
        let child = child.map_err(|error| ErrorKind::Node {
            parent_name: node.name,
            error,
        })?;

        let compatible = child
            .find_property("compatible")
            .map_err(ErrorKind::Prop)?
            .map(|prop| prop.read_str().map_err(ErrorKind::Prop))
            .transpose()?
            .unwrap_or("");

        if compatible == "microsoft,vmbus" {
            let vtl_name = igvm_defs::dt::IGVM_DT_VTL_PROPERTY;
            let vtl = child
                .find_property(vtl_name)
                .map_err(ErrorKind::Prop)?
                .ok_or(ErrorKind::PropMissing {
                    node_name: child.name,
                    prop_name: vtl_name,
                })?
                .read_u32(0)
                .map_err(ErrorKind::Prop)?;

            match vtl {
                0 => {
                    if vmbus_vtl0.replace(parse_vmbus(&child)?).is_some() {
                        return Err(ErrorKind::MultipleVmbusNode {
                            node_name: child.name,
                        });
                    }
                }
                2 => {
                    if vmbus_vtl2.replace(parse_vmbus(&child)?).is_some() {
                        return Err(ErrorKind::MultipleVmbusNode {
                            node_name: child.name,
                        });
                    }
                }
                _ => {
                    return Err(ErrorKind::UnexpectedVmbusVtl {
                        node_name: child.name,
                        vtl,
                    })
                }
            }
        }
    }

    Ok(())
}

fn parse_io_bus<'a>(
    node: &fdt::parser::Node<'a>,
    com3_serial: &mut bool,
) -> Result<(), ErrorKind<'a>> {
    for io_bus_child in node.children() {
        let io_bus_child = io_bus_child.map_err(|error| ErrorKind::Node {
            parent_name: node.name,
            error,
        })?;

        let compatible: &str = io_bus_child
            .find_property("compatible")
            .map_err(ErrorKind::Prop)?
            .map(|prop| prop.read_str().map_err(ErrorKind::Prop))
            .transpose()?
            .unwrap_or("");

        let _current_speed = io_bus_child
            .find_property("current-speed")
            .map_err(ErrorKind::Prop)?
            .ok_or(ErrorKind::PropMissing {
                node_name: io_bus_child.name,
                prop_name: "current-speed",
            })?
            .read_u32(0)
            .map_err(ErrorKind::Prop)?;

        let reg = io_bus_child
            .find_property("reg")
            .map_err(ErrorKind::Prop)?
            .ok_or(ErrorKind::PropMissing {
                node_name: io_bus_child.name,
                prop_name: "reg",
            })?;

        let reg_base = reg.read_u64(0).map_err(ErrorKind::Prop)?;
        let _reg_len = reg.read_u64(1).map_err(ErrorKind::Prop)?;

        // Linux kernel hard-codes COM3 to COM3_REG_BASE.
        // If work is ever done in the Linux kernel to instead
        // parse from DT, the 2nd condition can be removed.
        if compatible == "ns16550" && reg_base == COM3_REG_BASE {
            *com3_serial = true
        } else {
            #[cfg(feature = "tracing")]
            tracing::warn!(?node.name, ?compatible, ?reg_base,
                "unrecognized io bus child"
            );
        }
    }

    Ok(())
}

fn validate_property_str<'a>(
    child: &fdt::parser::Node<'a>,
    name: &'static str,
    expected: &'static str,
) -> Result<(), ErrorKind<'a>> {
    let actual = child
        .find_property(name)
        .map_err(ErrorKind::Prop)?
        .ok_or(ErrorKind::PropMissing {
            node_name: child.name,
            prop_name: name,
        })?
        .read_str()
        .map_err(ErrorKind::Prop)?;
    if actual != expected {
        return Err(ErrorKind::PropInvalidStr {
            node_name: child.name,
            prop_name: name,
            expected,
            actual,
        });
    }

    Ok(())
}

fn validate_property_u32<'a>(
    child: &fdt::parser::Node<'a>,
    name: &'static str,
    expected: u32,
    index: usize,
) -> Result<(), ErrorKind<'a>> {
    let actual = child
        .find_property(name)
        .map_err(ErrorKind::Prop)?
        .ok_or(ErrorKind::PropMissing {
            node_name: child.name,
            prop_name: name,
        })?
        .read_u32(index)
        .map_err(ErrorKind::Prop)?;
    if actual != expected {
        return Err(ErrorKind::PropInvalidU32 {
            node_name: child.name,
            prop_name: name,
            expected,
            actual,
        });
    }

    Ok(())
}

#[cfg(feature = "inspect")]
mod inspect_helpers {
    use super::*;

    pub(super) fn inspect_memory_map_entry_type(typ: &MemoryMapEntryType) -> impl Inspect + '_ {
        // TODO: inspect::AsDebug would work here once
        // https://github.com/kupiakos/open-enum/pull/13 is merged.
        inspect::adhoc(|req| match *typ {
            MemoryMapEntryType::MEMORY => req.value("MEMORY".into()),
            MemoryMapEntryType::PERSISTENT => req.value("PERSISTENT".into()),
            MemoryMapEntryType::PLATFORM_RESERVED => req.value("PLATFORM_RESERVED".into()),
            MemoryMapEntryType::VTL2_PROTECTABLE => req.value("VTL2_PROTECTABLE".into()),
            _ => req.value(typ.0.into()),
        })
    }

    pub(super) fn mmio_internal(mmio: &[MemoryRange]) -> impl Inspect + '_ {
        inspect::iter_by_key(
            mmio.iter()
                .map(|range| (range, inspect::AsHex(range.len()))),
        )
    }

    pub(super) fn memory_internal(memory: &[MemoryEntry]) -> impl Inspect + '_ {
        inspect::iter_by_key(memory.iter().map(|entry| (entry.range, entry)))
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use alloc::format;
    use alloc::vec;
    use alloc::vec::Vec;
    use fdt::builder::Builder;
    use fdt::builder::BuilderConfig;
    use fdt::builder::Nest;

    type TestParsedDeviceTree = ParsedDeviceTree<32, 32, 1024, 64>;

    fn new_vmbus_mmio(mmio: &[MemoryRange]) -> ArrayVec<MemoryRange, 2> {
        let mut vec = ArrayVec::new();
        vec.try_extend_from_slice(mmio).unwrap();
        vec
    }

    struct VmbusStringIds {
        p_address_cells: fdt::builder::StringId,
        p_size_cells: fdt::builder::StringId,
        p_compatible: fdt::builder::StringId,
        p_ranges: fdt::builder::StringId,
        p_vtl: fdt::builder::StringId,
        p_vmbus_connection_id: fdt::builder::StringId,
    }

    fn add_vmbus<'a>(
        ids: &VmbusStringIds,
        bus: Builder<'a, Nest<Nest<()>>>,
        vmbus_info: &VmbusInfo,
        vtl: u8,
    ) -> Builder<'a, Nest<Nest<()>>> {
        let mmio = {
            let mut ranges = Vec::new();
            for entry in &vmbus_info.mmio {
                ranges.push(entry.start());
                ranges.push(entry.start());
                ranges.push(entry.len());
            }
            ranges
        };
        let name = if mmio.is_empty() {
            format!("vmbus-vtl{vtl}")
        } else {
            format!("vmbus-vtl{vtl}@{:x}", mmio[0])
        };
        bus.start_node(&name)
            .unwrap()
            .add_u32(ids.p_address_cells, 2)
            .unwrap()
            .add_u32(ids.p_size_cells, 2)
            .unwrap()
            .add_str(ids.p_compatible, "microsoft,vmbus")
            .unwrap()
            .add_u64_array(ids.p_ranges, &mmio)
            .unwrap()
            .add_u32(ids.p_vtl, vtl as u32)
            .unwrap()
            .add_u32(ids.p_vmbus_connection_id, vmbus_info.connection_id)
            .unwrap()
            .end_node()
            .unwrap()
    }

    /// Build a dt from a parsed context.
    fn build_dt(context: &TestParsedDeviceTree) -> Vec<u8> {
        let mut buf = vec![0; 25600];
        let mut builder = Builder::new(BuilderConfig {
            blob_buffer: &mut buf,
            string_table_cap: 1024,
            memory_reservations: &[],
        })
        .expect("can build the DT builder");
        let p_address_cells = builder.add_string("#address-cells").unwrap();
        let p_size_cells = builder.add_string("#size-cells").unwrap();
        let p_model = builder.add_string("model").unwrap();
        let p_reg = builder.add_string("reg").unwrap();
        let p_ranges = builder.add_string("ranges").unwrap();
        let p_device_type = builder.add_string("device_type").unwrap();
        let p_status = builder.add_string("status").unwrap();
        let p_igvm_type = builder
            .add_string(igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY)
            .unwrap();
        let p_numa_node_id = builder.add_string("numa-node-id").unwrap();
        let p_compatible = builder.add_string("compatible").unwrap();
        let p_vmbus_connection_id = builder
            .add_string("microsoft,message-connection-id")
            .unwrap();
        let p_vtl = builder
            .add_string(igvm_defs::dt::IGVM_DT_VTL_PROPERTY)
            .unwrap();
        let p_bootargs = builder.add_string("bootargs").unwrap();
        let p_clock_frequency = builder.add_string("clock-frequency").unwrap();
        let p_current_speed = builder.add_string("current-speed").unwrap();
        let p_interrupts = builder.add_string("interrupts").unwrap();

        let mut cpus = builder
            .start_node("")
            .unwrap()
            .add_u32(p_address_cells, 2)
            .unwrap() // 64bit
            .add_u32(p_size_cells, 2)
            .unwrap() // 64bit
            .add_str(p_model, "microsoft,hyperv")
            .unwrap()
            .start_node("cpus")
            .unwrap()
            .add_u32(p_address_cells, 1)
            .unwrap()
            .add_u32(p_size_cells, 0)
            .unwrap();

        // Add a CPU node for each VP.
        for (index, cpu) in context.cpus.iter().enumerate() {
            let name = format!("cpu@{:x}", index);
            cpus = cpus
                .start_node(name.as_ref())
                .unwrap()
                .add_str(p_device_type, "cpu")
                .unwrap()
                .add_u32(p_reg, cpu.reg as u32)
                .unwrap()
                .add_u32(p_numa_node_id, cpu.vnode)
                .unwrap()
                .add_str(p_status, "okay")
                .unwrap()
                .end_node()
                .unwrap();
        }

        let mut root = cpus.end_node().unwrap();

        // Add memory, but reverse to test parsing sorting.
        // TODO: maybe shuffle order even more?
        for MemoryEntry {
            range,
            mem_type,
            vnode,
        } in context.memory.iter().rev()
        {
            let name = format!("memory@{:x}", range.start());
            root = root
                .start_node(name.as_ref())
                .unwrap()
                .add_str(p_device_type, "memory")
                .unwrap()
                .add_u64_array(p_reg, &[range.start(), range.len()])
                .unwrap()
                .add_u32(p_igvm_type, mem_type.0 as u32)
                .unwrap()
                .add_u32(p_numa_node_id, *vnode)
                .unwrap()
                .end_node()
                .unwrap();
        }

        // GIC
        if let Some(gic) = &context.gic {
            let p_interrupt_cells = root.add_string("#interrupt-cells").unwrap();
            let p_redist_regions = root.add_string("#redistributor-regions").unwrap();
            let p_redist_stride = root.add_string("redistributor-stride").unwrap();
            let p_interrupt_controller = root.add_string("interrupt-controller").unwrap();
            let p_phandle = root.add_string("phandle").unwrap();
            let name = format!("intc@{}", gic.gic_distributor_base);
            root = root
                .start_node(name.as_ref())
                .unwrap()
                .add_str(p_compatible, "arm,gic-v3")
                .unwrap()
                .add_u32(p_redist_regions, 1)
                .unwrap()
                .add_u64(p_redist_stride, gic.gic_redistributor_stride)
                .unwrap()
                .add_u64_array(
                    p_reg,
                    &[
                        gic.gic_distributor_base,
                        gic.gic_distributor_size,
                        gic.gic_redistributors_base,
                        gic.gic_redistributors_size,
                    ],
                )
                .unwrap()
                .add_u32(p_address_cells, 2)
                .unwrap()
                .add_u32(p_size_cells, 2)
                .unwrap()
                .add_u32(p_interrupt_cells, 3)
                .unwrap()
                .add_null(p_interrupt_controller)
                .unwrap()
                .add_u32(p_phandle, 1)
                .unwrap()
                .add_null(p_ranges)
                .unwrap()
                .end_node()
                .unwrap();
        }

        // Linux requires vmbus to be under a simple-bus node.
        let mut simple_bus = root
            .start_node("bus")
            .unwrap()
            .add_str(p_compatible, "simple-bus")
            .unwrap()
            .add_u32(p_address_cells, 2)
            .unwrap()
            .add_u32(p_size_cells, 2)
            .unwrap()
            .add_prop_array(p_ranges, &[])
            .unwrap();

        let vmbus_ids = VmbusStringIds {
            p_address_cells,
            p_size_cells,
            p_compatible,
            p_ranges,
            p_vtl,
            p_vmbus_connection_id,
        };

        // VTL0 vmbus root device
        if let Some(vmbus) = &context.vmbus_vtl0 {
            simple_bus = add_vmbus(&vmbus_ids, simple_bus, vmbus, 0);
        }

        // VTL2 vmbus root device
        if let Some(vmbus) = &context.vmbus_vtl2 {
            simple_bus = add_vmbus(&vmbus_ids, simple_bus, vmbus, 2);
        }

        root = simple_bus.end_node().unwrap();

        // Com3 serial node
        if context.com3_serial {
            let mut io_port_bus = root
                .start_node("io-bus")
                .unwrap()
                .add_str(p_compatible, "x86-pio-bus")
                .unwrap()
                .add_u32(p_address_cells, 1)
                .unwrap()
                .add_u32(p_size_cells, 0)
                .unwrap()
                .add_prop_array(p_ranges, &[])
                .unwrap();

            let serial_name = format!("serial@{:x}", COM3_REG_BASE);
            io_port_bus = io_port_bus
                .start_node(&serial_name)
                .unwrap()
                .add_str(p_compatible, "ns16550")
                .unwrap()
                .add_u32(p_clock_frequency, 0)
                .unwrap()
                .add_u32(p_current_speed, 115200)
                .unwrap()
                .add_u64_array(p_reg, &[COM3_REG_BASE, 0x8])
                .unwrap()
                .add_u64_array(p_interrupts, &[4])
                .unwrap()
                .end_node()
                .unwrap();

            root = io_port_bus.end_node().unwrap();
        }

        // Chosen node - contains cmdline.
        root = root
            .start_node("chosen")
            .unwrap()
            .add_str(p_bootargs, context.command_line.as_ref())
            .unwrap()
            .end_node()
            .unwrap();

        // openhcl node - contains openhcl specific information.
        let p_memory_allocation_mode = root.add_string("memory-allocation-mode").unwrap();
        let p_memory_allocation_size = root.add_string("memory-size").unwrap();
        let p_mmio_allocation_size = root.add_string("mmio-size").unwrap();
        let p_device_dma_page_count = root.add_string("total-pages").unwrap();
        let mut openhcl = root.start_node("openhcl").unwrap();

        let memory_alloc_str = match context.memory_allocation_mode {
            MemoryAllocationMode::Host => "host",
            MemoryAllocationMode::Vtl2 {
                memory_size,
                mmio_size,
            } => {
                // Encode the size at the expected property.
                if let Some(memory_size) = memory_size {
                    openhcl = openhcl
                        .add_u64(p_memory_allocation_size, memory_size)
                        .unwrap();
                }
                if let Some(mmio_size) = mmio_size {
                    openhcl = openhcl.add_u64(p_mmio_allocation_size, mmio_size).unwrap();
                }
                "vtl2"
            }
        };

        openhcl = openhcl
            .add_str(p_memory_allocation_mode, memory_alloc_str)
            .unwrap();

        // add device_dma_page_count
        if let Some(device_dma_page_count) = context.device_dma_page_count {
            openhcl = openhcl
                .start_node("device-dma")
                .unwrap()
                .add_u64(p_device_dma_page_count, device_dma_page_count)
                .unwrap()
                .end_node()
                .unwrap();
        }

        root = openhcl.end_node().unwrap();

        let bytes_used = root
            .end_node()
            .unwrap()
            .build(context.boot_cpuid_phys)
            .unwrap();
        buf.truncate(bytes_used);

        buf
    }

    /// Creates a parsed device tree context. No validation is performed.
    fn create_parsed(
        dt_size: usize,
        memory: &[MemoryEntry],
        cpus: &[CpuEntry],
        bsp: u32,
        vmbus_vtl0: Option<VmbusInfo>,
        vmbus_vtl2: Option<VmbusInfo>,
        command_line: &str,
        com3_serial: bool,
        gic: Option<GicInfo>,
        memory_allocation_mode: MemoryAllocationMode,
        device_dma_page_count: Option<u64>,
    ) -> TestParsedDeviceTree {
        let mut context = TestParsedDeviceTree::new();
        context.device_tree_size = dt_size;
        context.boot_cpuid_phys = bsp;
        write!(context.command_line, "{command_line}").unwrap();
        context.com3_serial = com3_serial;
        context.vmbus_vtl0 = vmbus_vtl0;
        context.vmbus_vtl2 = vmbus_vtl2;
        context.memory.try_extend_from_slice(memory).unwrap();
        context.cpus.try_extend_from_slice(cpus).unwrap();
        context.gic = gic;
        context.memory_allocation_mode = memory_allocation_mode;
        context.device_dma_page_count = device_dma_page_count;
        context
    }

    #[test]
    fn test_basic_dt() {
        let orig = create_parsed(
            2608,
            &[
                MemoryEntry {
                    range: MemoryRange::try_new(0..(1024 * HV_PAGE_SIZE)).unwrap(),
                    mem_type: MemoryMapEntryType::MEMORY,
                    vnode: 0,
                },
                MemoryEntry {
                    range: MemoryRange::try_new((1024 * HV_PAGE_SIZE)..(4024 * HV_PAGE_SIZE))
                        .unwrap(),
                    mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                    vnode: 0,
                },
                MemoryEntry {
                    range: MemoryRange::try_new((14024 * HV_PAGE_SIZE)..(102400 * HV_PAGE_SIZE))
                        .unwrap(),
                    mem_type: MemoryMapEntryType::MEMORY,
                    vnode: 0,
                },
            ],
            &[
                CpuEntry { reg: 12, vnode: 0 },
                CpuEntry { reg: 42, vnode: 0 },
                CpuEntry { reg: 23, vnode: 0 },
                CpuEntry { reg: 24, vnode: 0 },
            ],
            42,
            Some(VmbusInfo {
                mmio: new_vmbus_mmio(&[
                    MemoryRange::try_new((4024 * HV_PAGE_SIZE)..(4096 * HV_PAGE_SIZE)).unwrap(),
                    MemoryRange::try_new((102400 * HV_PAGE_SIZE)..(102800 * HV_PAGE_SIZE)).unwrap(),
                ]),
                connection_id: 1,
            }),
            Some(VmbusInfo {
                mmio: new_vmbus_mmio(&[MemoryRange::try_new(
                    (102800 * HV_PAGE_SIZE)..(102900 * HV_PAGE_SIZE),
                )
                .unwrap()]),
                connection_id: 4,
            }),
            "THIS_IS_A_BOOT_ARG=1",
            false,
            Some(GicInfo {
                gic_distributor_base: 0x20000,
                gic_distributor_size: 0x10000,
                gic_redistributors_base: 0x40000,
                gic_redistributors_size: 0x60000,
                gic_redistributor_stride: 0x20000,
            }),
            MemoryAllocationMode::Host,
            Some(1234),
        );

        let dt = build_dt(&orig);
        let mut parsed = TestParsedDeviceTree::new();
        let parsed = TestParsedDeviceTree::parse(&dt, &mut parsed).unwrap();
        assert_eq!(&orig, parsed);
    }

    #[test]
    fn test_numa_dt() {
        let orig = create_parsed(
            2352,
            &[
                MemoryEntry {
                    range: MemoryRange::try_new(0..(1024 * HV_PAGE_SIZE)).unwrap(),
                    mem_type: MemoryMapEntryType::MEMORY,
                    vnode: 0,
                },
                MemoryEntry {
                    range: MemoryRange::try_new((1024 * HV_PAGE_SIZE)..(2048 * HV_PAGE_SIZE))
                        .unwrap(),
                    mem_type: MemoryMapEntryType::MEMORY,
                    vnode: 1,
                },
                MemoryEntry {
                    range: MemoryRange::try_new((2048 * HV_PAGE_SIZE)..(3072 * HV_PAGE_SIZE))
                        .unwrap(),
                    mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                    vnode: 0,
                },
                MemoryEntry {
                    range: MemoryRange::try_new((3072 * HV_PAGE_SIZE)..(4096 * HV_PAGE_SIZE))
                        .unwrap(),
                    mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                    vnode: 1,
                },
                MemoryEntry {
                    range: MemoryRange::try_new((4096 * HV_PAGE_SIZE)..(51200 * HV_PAGE_SIZE))
                        .unwrap(),
                    mem_type: MemoryMapEntryType::MEMORY,
                    vnode: 0,
                },
                MemoryEntry {
                    range: MemoryRange::try_new((51200 * HV_PAGE_SIZE)..(102400 * HV_PAGE_SIZE))
                        .unwrap(),
                    mem_type: MemoryMapEntryType::MEMORY,
                    vnode: 1,
                },
            ],
            &[
                CpuEntry { reg: 12, vnode: 0 },
                CpuEntry { reg: 42, vnode: 1 },
                CpuEntry { reg: 23, vnode: 0 },
                CpuEntry { reg: 24, vnode: 1 },
            ],
            23,
            None,
            None,
            "",
            false,
            None,
            MemoryAllocationMode::Vtl2 {
                memory_size: Some(1000 * 1024 * 1024), // 1000 MB
                mmio_size: Some(128 * 1024 * 1024),    // 128 MB
            },
            None,
        );

        let dt = build_dt(&orig);
        let mut parsed = TestParsedDeviceTree::new();
        let parsed = TestParsedDeviceTree::parse(&dt, &mut parsed).unwrap();
        assert_eq!(&orig, parsed);
    }

    /// Tests memory ranges that overlap each other, or memory ranges that
    /// overlap vmbus mmio.
    #[test]
    fn test_overlapping_memory() {
        // mem overlaps each other
        let bad = create_parsed(
            0,
            &[
                MemoryEntry {
                    range: MemoryRange::try_new(0..(1024 * HV_PAGE_SIZE)).unwrap(),
                    mem_type: MemoryMapEntryType::MEMORY,
                    vnode: 0,
                },
                MemoryEntry {
                    range: MemoryRange::try_new(4096..(1024 * HV_PAGE_SIZE)).unwrap(),
                    mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                    vnode: 0,
                },
                MemoryEntry {
                    range: MemoryRange::try_new((14024 * HV_PAGE_SIZE)..(102400 * HV_PAGE_SIZE))
                        .unwrap(),
                    mem_type: MemoryMapEntryType::MEMORY,
                    vnode: 0,
                },
            ],
            &[
                CpuEntry { reg: 12, vnode: 0 },
                CpuEntry { reg: 42, vnode: 0 },
                CpuEntry { reg: 23, vnode: 0 },
                CpuEntry { reg: 24, vnode: 0 },
            ],
            42,
            None,
            None,
            "THIS_IS_A_BOOT_ARG=1",
            false,
            None,
            MemoryAllocationMode::Host,
            None,
        );

        let dt = build_dt(&bad);
        let mut parsed = TestParsedDeviceTree::new();
        assert!(matches!(
            TestParsedDeviceTree::parse(&dt, &mut parsed),
            Err(Error(ErrorKind::MemoryRegOverlap { .. }))
        ));

        // mem contained within another
        let bad = create_parsed(
            0,
            &[
                MemoryEntry {
                    range: MemoryRange::try_new(4096..(1024 * HV_PAGE_SIZE)).unwrap(),
                    mem_type: MemoryMapEntryType::MEMORY,
                    vnode: 0,
                },
                MemoryEntry {
                    range: MemoryRange::try_new(0..(102400 * HV_PAGE_SIZE)).unwrap(),
                    mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                    vnode: 0,
                },
            ],
            &[
                CpuEntry { reg: 12, vnode: 0 },
                CpuEntry { reg: 42, vnode: 0 },
                CpuEntry { reg: 23, vnode: 0 },
                CpuEntry { reg: 24, vnode: 0 },
            ],
            42,
            None,
            None,
            "THIS_IS_A_BOOT_ARG=1",
            false,
            None,
            MemoryAllocationMode::Host,
            None,
        );

        let dt = build_dt(&bad);
        let mut parsed = TestParsedDeviceTree::new();
        assert!(matches!(
            TestParsedDeviceTree::parse(&dt, &mut parsed),
            Err(Error(ErrorKind::MemoryRegOverlap { .. }))
        ));

        // mem overlaps vmbus
        let bad = create_parsed(
            0,
            &[MemoryEntry {
                range: MemoryRange::try_new(0..(202400 * HV_PAGE_SIZE)).unwrap(),
                mem_type: MemoryMapEntryType::MEMORY,
                vnode: 0,
            }],
            &[
                CpuEntry { reg: 12, vnode: 0 },
                CpuEntry { reg: 42, vnode: 0 },
                CpuEntry { reg: 23, vnode: 0 },
                CpuEntry { reg: 24, vnode: 0 },
            ],
            42,
            Some(VmbusInfo {
                mmio: new_vmbus_mmio(&[MemoryRange::try_new(
                    (4024 * HV_PAGE_SIZE)..(4096 * HV_PAGE_SIZE),
                )
                .unwrap()]),
                connection_id: 1,
            }),
            Some(VmbusInfo {
                mmio: new_vmbus_mmio(&[MemoryRange::try_new(
                    (102800 * HV_PAGE_SIZE)..(102900 * HV_PAGE_SIZE),
                )
                .unwrap()]),
                connection_id: 4,
            }),
            "THIS_IS_A_BOOT_ARG=1",
            false,
            None,
            MemoryAllocationMode::Host,
            None,
        );

        let dt = build_dt(&bad);
        let mut parsed = TestParsedDeviceTree::new();
        assert!(matches!(
            TestParsedDeviceTree::parse(&dt, &mut parsed),
            Err(Error(ErrorKind::VmbusMmioOverlapsRam { .. }))
        ));

        // vmbus overlap each other
        let bad = create_parsed(
            0,
            &[MemoryEntry {
                range: MemoryRange::try_new(0..(1024 * HV_PAGE_SIZE)).unwrap(),
                mem_type: MemoryMapEntryType::MEMORY,
                vnode: 0,
            }],
            &[
                CpuEntry { reg: 12, vnode: 0 },
                CpuEntry { reg: 42, vnode: 0 },
                CpuEntry { reg: 23, vnode: 0 },
                CpuEntry { reg: 24, vnode: 0 },
            ],
            42,
            Some(VmbusInfo {
                mmio: new_vmbus_mmio(&[
                    MemoryRange::try_new((4000 * HV_PAGE_SIZE)..(4096 * HV_PAGE_SIZE)).unwrap(),
                    MemoryRange::EMPTY,
                ]),
                connection_id: 1,
            }),
            Some(VmbusInfo {
                mmio: new_vmbus_mmio(&[
                    MemoryRange::try_new((4020 * HV_PAGE_SIZE)..(102900 * HV_PAGE_SIZE)).unwrap(),
                    MemoryRange::EMPTY,
                ]),
                connection_id: 4,
            }),
            "THIS_IS_A_BOOT_ARG=1",
            false,
            None,
            MemoryAllocationMode::Host,
            None,
        );

        let dt = build_dt(&bad);
        let mut parsed = TestParsedDeviceTree::new();
        assert!(matches!(
            TestParsedDeviceTree::parse(&dt, &mut parsed),
            Err(Error(ErrorKind::VmbusMmioOverlapsVmbusMmio { .. }))
        ));
    }

    /// tests serial output
    #[test]
    fn test_com3_serial_output() {
        let orig = create_parsed(
            2560,
            &[
                MemoryEntry {
                    range: MemoryRange::try_new(0..(1024 * HV_PAGE_SIZE)).unwrap(),
                    mem_type: MemoryMapEntryType::MEMORY,
                    vnode: 0,
                },
                MemoryEntry {
                    range: MemoryRange::try_new((1024 * HV_PAGE_SIZE)..(4024 * HV_PAGE_SIZE))
                        .unwrap(),
                    mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                    vnode: 0,
                },
                MemoryEntry {
                    range: MemoryRange::try_new((14024 * HV_PAGE_SIZE)..(102400 * HV_PAGE_SIZE))
                        .unwrap(),
                    mem_type: MemoryMapEntryType::MEMORY,
                    vnode: 0,
                },
            ],
            &[
                CpuEntry { reg: 12, vnode: 0 },
                CpuEntry { reg: 42, vnode: 0 },
                CpuEntry { reg: 23, vnode: 0 },
                CpuEntry { reg: 24, vnode: 0 },
            ],
            42,
            Some(VmbusInfo {
                mmio: new_vmbus_mmio(&[
                    MemoryRange::try_new((4024 * HV_PAGE_SIZE)..(4096 * HV_PAGE_SIZE)).unwrap(),
                    MemoryRange::try_new((102400 * HV_PAGE_SIZE)..(102800 * HV_PAGE_SIZE)).unwrap(),
                ]),
                connection_id: 1,
            }),
            Some(VmbusInfo {
                mmio: new_vmbus_mmio(&[MemoryRange::try_new(
                    (102800 * HV_PAGE_SIZE)..(102900 * HV_PAGE_SIZE),
                )
                .unwrap()]),
                connection_id: 4,
            }),
            "THIS_IS_A_BOOT_ARG=1",
            true,
            None,
            MemoryAllocationMode::Host,
            None,
        );

        let dt = build_dt(&orig);
        let mut parsed = TestParsedDeviceTree::new();
        let parsed = TestParsedDeviceTree::parse(&dt, &mut parsed).unwrap();

        assert_eq!(&orig, parsed);
        assert!(parsed.com3_serial);
    }
}
