// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded conversion of the host's DeviceTree hardware topology to ACPI.
//!
//! The topology is unmeasured and is not attested. Attestation policy is deferred
//! for bring-up. This supports only native x86 MSI/MSI-X, node 0, and identity
//! memory windows. Unsupported features fail before Linux runs. The caller must
//! provide a stable private copy of the DT and measured workspace bounds.

use acpi_core::builder::{Builder, OemInfo, Table};
use acpi_core::ssdt::{PcieHostBridgeEntry, Ssdt};
use alloc::vec::Vec;
use fdt::parser::{Node, Parser, Property};
use memory_range::MemoryRange;
use zerocopy::{FromBytes, IntoBytes};

const MAX_DT_SIZE: usize = loader_defs::linux::SNP_BOOT_SHIM_DT_SIZE as usize;
const MAX_NODES: usize = 1024;
const MAX_DEPTH: usize = 16;
const MAX_PROPERTIES: usize = 32;
const MAX_BRIDGES: usize = loader_defs::linux::SNP_BOOT_SHIM_MAX_PCIE_BRIDGES;
const HEADER_SIZE: usize = size_of::<acpi_spec::Header>();
const BUS_SIZE: u64 = 1 << 20;
const FOUR_GB: u64 = 1 << 32;
const ARCH_MMIO_START: u64 = 0xfe00_0000;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error<'a> {
    // A borrowed parser error cannot implement Error::source's 'static contract.
    #[error("invalid DeviceTree: {0}")]
    Fdt(fdt::parser::Error<'a>),
    #[error("DeviceTree exceeds a size, node, depth, or property limit")]
    DtLimit,
    #[error("duplicate DeviceTree property: {0}")]
    DuplicateProperty(&'a str),
    #[error("missing or invalid DeviceTree property: {0}")]
    Property(&'static str),
    #[error("unsupported PCIe feature: {0}")]
    Unsupported(&'static str),
    #[error("more than eight PCIe host bridges")]
    TooManyBridges,
    #[error("duplicate PCI segment")]
    DuplicateSegment,
    #[error("invalid ECAM or MMIO range")]
    Range,
    #[error("ECAM or MMIO overlaps RAM, reserved MMIO, or another window")]
    Overlap,
    #[error("invalid physical address limit")]
    AddressLimit,
    #[error("invalid generated RSDP")]
    Rsdp,
    #[error("invalid generated ACPI table header, checksum, or pointer")]
    BaseAcpi,
    #[error("generated ACPI has ambiguous or unsupported tables")]
    BaseTopology,
    #[error("ACPI output exceeds its reserved capacity")]
    Capacity,
}

impl<'a> From<fdt::parser::Error<'a>> for Error<'a> {
    fn from(error: fdt::parser::Error<'a>) -> Self {
        Self::Fdt(error)
    }
}

pub(crate) struct Properties<'a> {
    entries: [Option<Property<'a>>; MAX_PROPERTIES],
}

impl<'a> Properties<'a> {
    pub(crate) fn read(node: &Node<'a>) -> Result<Self, Error<'a>> {
        let mut entries: [Option<Property<'a>>; MAX_PROPERTIES] = core::array::from_fn(|_| None);
        for (index, property) in node.properties().enumerate() {
            let property = property?;
            if index == MAX_PROPERTIES {
                return Err(Error::DtLimit);
            }
            if entries[..index]
                .iter()
                .flatten()
                .any(|previous| previous.name == property.name)
            {
                return Err(Error::DuplicateProperty(property.name));
            }
            entries[index] = Some(property);
        }
        Ok(Self { entries })
    }

    pub(crate) fn get(&self, name: &str) -> Option<&'a [u8]> {
        self.entries
            .iter()
            .flatten()
            .find(|property| property.name == name)
            .map(|property| property.data)
    }

    pub(crate) fn required(
        &self,
        name: &'static str,
        length: usize,
    ) -> Result<&'a [u8], Error<'a>> {
        self.get(name)
            .filter(|data| data.len() == length)
            .ok_or(Error::Property(name))
    }

    pub(crate) fn cell(&self, name: &'static str) -> Result<u32, Error<'a>> {
        Ok(be32(self.required(name, 4)?))
    }

    pub(crate) fn only(&self, allowed: &[&str]) -> Result<(), Error<'a>> {
        if self
            .entries
            .iter()
            .flatten()
            .any(|p| !allowed.contains(&p.name))
        {
            return Err(Error::Unsupported("unknown hardware property"));
        }
        Ok(())
    }
}

// These helpers receive fixed-width slices after their enclosing record has
// passed its length check.
fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub(crate) fn be64(bytes: &[u8]) -> u64 {
    (u64::from(be32(bytes)) << 32) | u64::from(be32(&bytes[4..]))
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn le64(bytes: &[u8]) -> u64 {
    u64::from(le32(bytes)) | (u64::from(le32(&bytes[4..])) << 32)
}

#[derive(Debug, Copy, Clone)]
struct Bridge {
    segment: u16,
    start_bus: u8,
    end_bus: u8,
    ecam: MemoryRange,
    low: MemoryRange,
    high: MemoryRange,
}

fn memory_window<'a>(
    base: u64,
    length: u64,
    ram_end: u64,
    limit: u64,
) -> Result<MemoryRange, Error<'a>> {
    let end = base.checked_add(length).ok_or(Error::Range)?;
    if base >= limit || end > limit {
        return Err(Error::AddressLimit);
    }
    let range = MemoryRange::try_new(base..end).map_err(|_| Error::Range)?;
    if !range.is_empty() && (base < ram_end || (base < FOUR_GB && end > ARCH_MMIO_START)) {
        return Err(Error::Overlap);
    }
    Ok(range)
}

fn parse_bridge<'a>(
    node: &Node<'a>,
    depth: usize,
    ram_end: u64,
    limit: u64,
) -> Result<Option<Bridge>, Error<'a>> {
    let props = Properties::read(node)?;
    // Linux can also receive this global policy under /chosen.
    if props.get("linux,pci-probe-only").is_some() && props.cell("linux,pci-probe-only")? != 0 {
        return Err(Error::Unsupported("preserve PCI boot configuration"));
    }
    if props
        .entries
        .iter()
        .flatten()
        .any(|property| property.name.contains("cxl"))
    {
        return Err(Error::Unsupported("CXL"));
    }
    let compatible = props.get("compatible");
    if let Some(compatible) = compatible {
        if compatible.is_empty()
            || compatible.last() != Some(&0)
            || compatible[..compatible.len() - 1]
                .split(|byte| *byte == 0)
                .any(|name| name.is_empty() || core::str::from_utf8(name).is_err())
        {
            return Err(Error::Property("compatible"));
        }
        if compatible.split(|byte| *byte == 0).any(|name| {
            name.windows(3)
                .any(|part| part.eq_ignore_ascii_case(b"cxl"))
        }) {
            return Err(Error::Unsupported("CXL"));
        }
    }
    let pci = props.get("device_type") == Some(b"pci\0".as_slice())
        || matches!(node.name.split('@').next(), Some("pci" | "pcie"))
        || compatible.is_some_and(|names| {
            names
                .split(|byte| *byte == 0)
                .any(|name| name == b"pci-host-ecam-generic")
        });
    if !pci {
        return Ok(None);
    }
    if depth != 1 {
        return Err(Error::Unsupported("PCIe host bridge is not a root child"));
    }
    if compatible != Some(b"pci-host-ecam-generic\0".as_slice()) {
        return Err(Error::Unsupported("PCIe compatible"));
    }
    if props.get("device_type") != Some(b"pci\0".as_slice()) {
        return Err(Error::Property("device_type"));
    }
    if props.cell("#address-cells")? != 3 || props.cell("#size-cells")? != 2 {
        return Err(Error::Property("PCI address/size cells"));
    }
    for name in [
        "iommu-map",
        "iommu-map-mask",
        "iommus",
        "msi-map",
        "msi-map-mask",
        "msi-parent",
        "interrupt-map",
        "interrupt-map-mask",
        "interrupt-parent",
        "interrupts",
        "interrupts-extended",
        "dma-ranges",
        "ats-supported",
        "external-facing",
    ] {
        if props.get(name).is_some() {
            return Err(Error::Unsupported(name));
        }
    }
    if let Some(status) = props.get("status") {
        if status != b"okay\0" && status != b"ok\0" {
            return Err(Error::Unsupported("disabled PCIe host bridge"));
        }
    }
    if props.get("numa-node-id").is_some() && props.cell("numa-node-id")? != 0 {
        return Err(Error::Unsupported("nonzero NUMA node"));
    }
    props.only(&[
        "compatible",
        "device_type",
        "#address-cells",
        "#size-cells",
        "linux,pci-domain",
        "bus-range",
        "reg",
        "ranges",
        "numa-node-id",
        "status",
        "linux,pci-probe-only",
    ])?;
    let segment = u16::try_from(props.cell("linux,pci-domain")?)
        .map_err(|_| Error::Property("linux,pci-domain"))?;
    let buses = props.required("bus-range", 8)?;
    let start_bus = u8::try_from(be32(buses)).map_err(|_| Error::Property("bus-range"))?;
    let end_bus = u8::try_from(be32(&buses[4..])).map_err(|_| Error::Property("bus-range"))?;
    if start_bus > end_bus {
        return Err(Error::Property("bus-range"));
    }
    let reg = props.required("reg", 16)?;
    let ecam_base = be64(reg);
    let ecam_size = be64(&reg[8..]);
    if ecam_base < 256 * BUS_SIZE
        || !ecam_base.is_multiple_of(BUS_SIZE)
        || ecam_size != (u64::from(end_bus) - u64::from(start_bus) + 1) * BUS_SIZE
        || ecam_base
            .checked_sub(u64::from(start_bus) * BUS_SIZE)
            .is_none()
    {
        return Err(Error::Range);
    }
    let ecam = memory_window(ecam_base, ecam_size, ram_end, limit)?;
    let ranges = props.get("ranges").ok_or(Error::Property("ranges"))?;
    if ranges.is_empty() || !ranges.len().is_multiple_of(28) || ranges.len() > 56 {
        return Err(Error::Property("ranges"));
    }
    let mut low = None;
    let mut high = None;
    for range in ranges.chunks_exact(28) {
        let flags = be32(range);
        let child = be64(&range[4..]);
        let parent = be64(&range[12..]);
        let size = be64(&range[20..]);
        if child != parent {
            return Err(Error::Unsupported("non-identity PCI memory translation"));
        }
        // The ACPI builder emits non-prefetchable memory resources. Do not
        // discard the prefetch bit, IO-space flags, or other PCI address flags.
        let slot = match flags {
            0x0200_0000 => &mut low,
            0x0300_0000 => &mut high,
            _ => return Err(Error::Unsupported("PCI range flags")),
        };
        if slot.is_some() {
            return Err(Error::Property("duplicate PCI range kind"));
        }
        let window = memory_window(parent, size, ram_end, limit)?;
        if flags == 0x0200_0000 {
            if window.is_empty() || window.end() > FOUR_GB {
                return Err(Error::Range);
            }
        } else if !window.is_empty() && window.start() < FOUR_GB {
            return Err(Error::Range);
        }
        *slot = Some(window);
    }
    // A nonempty MEM32 window is required. An omitted or zero-length MEM64
    // window means no high MMIO resource; it does not create a zero-size AML range.
    Ok(Some(Bridge {
        segment,
        start_bus,
        end_bus,
        ecam,
        low: low.ok_or(Error::Property("MEM32 range"))?,
        high: high.unwrap_or(MemoryRange::EMPTY),
    }))
}

fn parse_bridges<'a>(
    dt: &'a [u8],
    ram_end: u64,
    limit: u64,
) -> Result<[Option<Bridge>; MAX_BRIDGES], Error<'a>> {
    let total = Parser::read_total_size(dt)?;
    if total > MAX_DT_SIZE {
        return Err(Error::DtLimit);
    }
    let parser = Parser::new(dt.get(..total).ok_or(Error::DtLimit)?)?;
    let root = parser.root()?;
    let root_props = Properties::read(&root)?;
    if !root.name.is_empty()
        || root_props.cell("#address-cells")? != 2
        || root_props.cell("#size-cells")? != 2
    {
        return Err(Error::Property("root address/size cells"));
    }
    if root_props.get("linux,pci-probe-only").is_some()
        && root_props.cell("linux,pci-probe-only")? != 0
    {
        return Err(Error::Unsupported("preserve PCI boot configuration"));
    }
    let mut bridges: [Option<Bridge>; MAX_BRIDGES] = [None; MAX_BRIDGES];
    let mut bridge_count = 0;
    let mut stack: [Option<fdt::parser::NodeIter<'a>>; MAX_DEPTH] = core::array::from_fn(|_| None);
    stack[0] = Some(root.children());
    let mut depth = 1;
    let mut nodes = 1;
    while depth != 0 {
        let children = stack[depth - 1].as_mut().ok_or(Error::DtLimit)?;
        let Some(node) = children.next() else {
            stack[depth - 1] = None;
            depth -= 1;
            continue;
        };
        let node = node?;
        nodes += 1;
        if nodes > MAX_NODES || depth >= MAX_DEPTH {
            return Err(Error::DtLimit);
        }
        if let Some(bridge) = parse_bridge(&node, depth, ram_end, limit)? {
            if bridge_count == MAX_BRIDGES {
                return Err(Error::TooManyBridges);
            }
            if bridges
                .iter()
                .flatten()
                .any(|previous| previous.segment == bridge.segment)
            {
                return Err(Error::DuplicateSegment);
            }
            bridges[bridge_count] = Some(bridge);
            bridge_count += 1;
        }
        stack[depth] = Some(node.children());
        depth += 1;
    }
    let mut windows = [MemoryRange::EMPTY; MAX_BRIDGES * 3];
    let mut count = 0;
    for bridge in bridges.iter().flatten() {
        for window in [bridge.ecam, bridge.low, bridge.high] {
            if window.is_empty() {
                continue;
            }
            if windows[..count]
                .iter()
                .any(|other| window.start() < other.end() && other.start() < window.end())
            {
                return Err(Error::Overlap);
            }
            windows[count] = window;
            count += 1;
        }
    }
    Ok(bridges)
}

fn validate_address_limit<'a>(ram_end: u64, c_bit_mask: u64) -> Result<(), Error<'a>> {
    if !c_bit_mask.is_power_of_two()
        || c_bit_mask > 1 << 51
        || ram_end == 0
        || ram_end > c_bit_mask
        || !ram_end.is_multiple_of(4096)
    {
        return Err(Error::AddressLimit);
    }
    Ok(())
}

/// Validate host PCIe windows before accepting any omitted RAM.
///
/// This path must not allocate: the temporary heap is itself still unaccepted
/// RAM. The DT parameter is already imported as private unmeasured pages.
pub(crate) fn validate_device_tree(
    dt: &[u8],
    expected_cpu_count: u32,
    ram_end: u64,
    c_bit_mask: u64,
) -> Result<(), Error<'_>> {
    validate_address_limit(ram_end, c_bit_mask)?;
    parse_bridges(dt, ram_end, c_bit_mask)?;
    crate::topology::parse(dt, expected_cpu_count, ram_end)?;
    Ok(())
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
}

fn base_table(base: &[u8], base_gpa: u64, address: u64) -> Result<&[u8], Error<'static>> {
    let offset = address
        .checked_sub(base_gpa)
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or(Error::BaseAcpi)?;
    let bytes = base.get(offset..).ok_or(Error::BaseAcpi)?;
    let (header, _) = acpi_spec::Header::read_from_prefix(bytes).map_err(|_| Error::BaseAcpi)?;
    let length = header.length.get() as usize;
    if length < HEADER_SIZE {
        return Err(Error::BaseAcpi);
    }
    let table = bytes.get(..length).ok_or(Error::BaseAcpi)?;
    if checksum(table) != 0 {
        return Err(Error::BaseAcpi);
    }
    Ok(table)
}

fn validate_fadt(base: &[u8], base_gpa: u64, fadt: &[u8]) -> Result<(), Error<'static>> {
    if fadt.len() < 44 {
        return Err(Error::BaseAcpi);
    }
    let dsdt = u64::from(le32(&fadt[40..]));
    let x_dsdt = if fadt.len() >= 148 {
        le64(&fadt[140..])
    } else {
        0
    };
    if (dsdt != 0 && x_dsdt != 0 && dsdt != x_dsdt)
        || le32(&fadt[36..]) != 0
        || (fadt.len() >= 140 && le64(&fadt[132..]) != 0)
    {
        return Err(Error::BaseTopology);
    }
    let address = if x_dsdt != 0 { x_dsdt } else { dsdt };
    if address == 0 || &base_table(base, base_gpa, address)?[..4] != b"DSDT" {
        return Err(Error::BaseAcpi);
    }
    Ok(())
}

fn validate_output(
    tables: &[u8],
    output_gpa: u64,
    rsdp_bytes: &[u8],
    bridge_count: usize,
) -> Result<(), Error<'static>> {
    let rsdp = acpi_spec::Rsdp::read_from_bytes(rsdp_bytes).map_err(|_| Error::Rsdp)?;
    if rsdp.signature != *b"RSD PTR "
        || rsdp.revision != 2
        || rsdp.length as usize != size_of::<acpi_spec::Rsdp>()
        || checksum(&rsdp_bytes[..20]) != 0
        || checksum(rsdp_bytes) != 0
        || rsdp.rsdt != 0
        || rsdp.rsvd != [0; 3]
    {
        return Err(Error::Rsdp);
    }
    let xsdt = base_table(tables, output_gpa, rsdp.xsdt)?;
    if &xsdt[..4] != b"XSDT" {
        return Err(Error::BaseAcpi);
    }
    let entries = &xsdt[HEADER_SIZE..];
    let signatures: &[&[u8; 4]] = if bridge_count == 0 {
        &[b"FACP", b"APIC", b"SRAT"]
    } else {
        &[b"FACP", b"APIC", b"SRAT", b"MCFG", b"SSDT"]
    };
    if entries.len() != signatures.len() * 8 {
        return Err(Error::BaseAcpi);
    }
    for (index, bytes) in entries.chunks_exact(8).enumerate() {
        let address = le64(bytes);
        let table = base_table(tables, output_gpa, address)?;
        if &table[..4] != signatures[index].as_slice() {
            return Err(Error::BaseTopology);
        }
        if index == 0 {
            validate_fadt(tables, output_gpa, table)?;
        }
    }
    Ok(())
}

/// Returns RSDP bytes and output-region table bytes, as in `Builder::build`.
///
/// All final pointers refer to the permanent output region. Capacity covers table bytes, not
/// the separately returned RSDP. The caller bounds the allocator independently.
pub(crate) fn build_acpi<'a>(
    dt: &'a [u8],
    expected_cpu_count: u32,
    output_gpa: u64,
    output_capacity: usize,
    ram_end: u64,
    c_bit_mask: u64,
) -> Result<(Vec<u8>, Vec<u8>), Error<'a>> {
    validate_address_limit(ram_end, c_bit_mask)?;
    let output_end = output_gpa
        .checked_add(output_capacity as u64)
        .ok_or(Error::Capacity)?;
    if output_gpa == 0
        || output_end > ram_end
        || output_capacity > loader_defs::linux::SNP_BOOT_SHIM_ACPI_SIZE as usize
        || !output_gpa.is_multiple_of(8)
    {
        return Err(Error::BaseAcpi);
    }
    let bridges = parse_bridges(dt, ram_end, c_bit_mask)?;
    let topology = crate::topology::parse(dt, expected_cpu_count, ram_end)?;
    let oem = OemInfo {
        oem_id: *b"HVLITE",
        oem_tableid: *b"HVLITETB",
        oem_revision: 0,
        creator_id: *b"MSHV",
        creator_revision: 0,
    };
    let bridge_count = bridges.iter().flatten().count();
    let base = topology.build(&oem);
    let fadt = base.fadt(output_gpa, &oem).map_err(|_| Error::BaseAcpi)?;
    let mut required =
        (HEADER_SIZE + (3 + if bridge_count == 0 { 0 } else { 2 }) * 8).next_multiple_of(8);
    for table in [&base.dsdt, &fadt, &base.madt, &base.srat] {
        required += table.len().next_multiple_of(8);
    }
    if required > output_capacity {
        return Err(Error::Capacity);
    }
    let mut builder = Builder::new(output_gpa, oem);
    builder.append_raw(&base.dsdt);
    builder.append_raw(&fadt);
    builder.append_raw(&base.madt);
    builder.append_raw(&base.srat);
    if bridge_count != 0 {
        let mut ssdt = Ssdt::new();
        let mut segments = Vec::with_capacity(bridge_count);
        for (index, bridge) in bridges.iter().flatten().enumerate() {
            ssdt.add_pcie(PcieHostBridgeEntry {
                index: index as u32,
                segment: bridge.segment,
                start_bus: bridge.start_bus,
                end_bus: bridge.end_bus,
                ecam_range: bridge.ecam,
                low_mmio: bridge.low,
                high_mmio: bridge.high,
                cxl: false,
                vnode: Some(0),
                preserve_boot_config: false,
            });
            let bus_zero = bridge
                .ecam
                .start()
                .checked_sub(u64::from(bridge.start_bus) * BUS_SIZE)
                .ok_or(Error::Range)?;
            segments.push(acpi_spec::mcfg::McfgSegmentBusRange::new(
                bus_zero,
                bridge.segment,
                bridge.start_bus,
                bridge.end_bus,
            ));
        }
        let ssdt = ssdt.to_bytes();
        let mcfg = acpi_spec::mcfg::McfgHeader::new();
        required += ssdt.len().next_multiple_of(8)
            + (HEADER_SIZE + size_of_val(&mcfg) + size_of_val(segments.as_slice()))
                .next_multiple_of(8);
        if required > output_capacity {
            return Err(Error::Capacity);
        }
        builder.append(&Table::new_dyn(
            acpi_spec::mcfg::MCFG_REVISION,
            None,
            &mcfg,
            &[segments.as_bytes()],
        ));
        builder.append_raw(&ssdt);
    }
    if required > output_capacity {
        return Err(Error::Capacity);
    }
    let result = builder.build();
    if result.1.len() > output_capacity {
        return Err(Error::Capacity);
    }
    validate_output(&result.1, output_gpa, &result.0, bridge_count)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpi_core::ssdt::AmlObject;
    use alloc::vec;
    use test_with_tracing::test;

    const OUTPUT: u64 = 0x20_0000;
    const RAM_END: u64 = 0x1000_0000;
    const CAPACITY: usize = 64 * 1024;
    const C_BIT: u64 = 1 << 47;

    type TestProperties = Vec<(&'static str, Vec<u8>)>;

    #[derive(Clone)]
    struct TestNode {
        name: String,
        properties: TestProperties,
        children: Vec<TestNode>,
    }

    fn node(name: String, properties: TestProperties) -> TestNode {
        TestNode {
            name,
            properties,
            children: vec![],
        }
    }

    fn hardware(cpu_count: usize, ram_count: usize, uart_mask: u8) -> Vec<TestNode> {
        let mut cpus = node(
            "cpus".into(),
            vec![
                ("#address-cells", cells(&[1])),
                ("#size-cells", cells(&[0])),
            ],
        );
        for index in 0..cpu_count {
            cpus.children.push(node(
                alloc::format!("cpu@{:x}", index + 1),
                vec![
                    ("device_type", b"cpu\0".to_vec()),
                    ("reg", cells(&[index as u32])),
                    ("numa-node-id", cells(&[0])),
                    ("status", b"okay\0".to_vec()),
                ],
            ));
        }
        let mut nodes = vec![cpus];
        let mut start = 0;
        for index in 0..ram_count {
            let end = if index + 1 == ram_count {
                RAM_END
            } else {
                start + 4096
            };
            nodes.push(node(
                alloc::format!("memory@{start:x}"),
                vec![
                    ("device_type", b"memory\0".to_vec()),
                    (
                        "reg",
                        [start.to_be_bytes(), (end - start).to_be_bytes()].concat(),
                    ),
                    ("numa-node-id", cells(&[0])),
                    (
                        igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY,
                        cells(&[u32::from(igvm_defs::MemoryMapEntryType::MEMORY.0)]),
                    ),
                ],
            ));
            start = end;
        }
        if uart_mask != 0 {
            let mut pio = node(
                "pio-bus".into(),
                vec![
                    ("compatible", b"x86-pio-bus\0".to_vec()),
                    ("#address-cells", cells(&[1])),
                    ("#size-cells", cells(&[1])),
                    ("ranges", vec![]),
                ],
            );
            for index in 0..4 {
                if uart_mask & (1 << index) == 0 {
                    continue;
                }
                let base = u64::from(x86defs::serial::COM_BASES[index]);
                pio.children.push(node(
                    alloc::format!("serial@{base:x}"),
                    vec![
                        ("compatible", b"ns16550\0".to_vec()),
                        ("reg", [base.to_be_bytes(), 8u64.to_be_bytes()].concat()),
                        (
                            "interrupts",
                            u64::from(x86defs::serial::COM_IRQS[index])
                                .to_be_bytes()
                                .to_vec(),
                        ),
                        ("current-speed", cells(&[115200])),
                        ("clock-frequency", cells(&[0])),
                    ],
                ));
            }
            nodes.push(pio);
        }
        nodes
    }

    fn emit<'a, N>(
        mut builder: fdt::builder::Builder<'a, N>,
        nodes: &[TestNode],
        names: &std::collections::BTreeMap<&str, fdt::builder::StringId>,
    ) -> fdt::builder::Builder<'a, N> {
        for node in nodes {
            let mut child = builder.start_node(&node.name).unwrap();
            for (name, data) in &node.properties {
                child = child.add_prop_array(names[name], &[data]).unwrap();
            }
            for node in &node.children {
                assert!(node.children.is_empty());
                let mut leaf = child.start_node(&node.name).unwrap();
                for (name, data) in &node.properties {
                    leaf = leaf.add_prop_array(names[name], &[data]).unwrap();
                }
                child = leaf.end_node().unwrap();
            }
            builder = child.end_node().unwrap();
        }
        builder
    }

    fn root_properties() -> TestProperties {
        vec![
            ("#address-cells", cells(&[2])),
            ("#size-cells", cells(&[2])),
        ]
    }

    fn cells(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect()
    }

    fn bridge(index: u32) -> TestProperties {
        vec![
            ("compatible", b"pci-host-ecam-generic\0".to_vec()),
            ("device_type", b"pci\0".to_vec()),
            ("#address-cells", cells(&[3])),
            ("#size-cells", cells(&[2])),
            ("linux,pci-domain", cells(&[index])),
            ("numa-node-id", cells(&[0])),
            (
                "reg",
                cells(&[0, 0x4000_0000 + index * 0x0100_0000, 0, 0x0100_0000]),
            ),
            ("bus-range", cells(&[0, 15])),
            (
                "ranges",
                cells(&[
                    0x0200_0000,
                    0,
                    0x8000_0000 + index * 0x0100_0000,
                    0,
                    0x8000_0000 + index * 0x0100_0000,
                    0,
                    0x0100_0000,
                    0x0300_0000,
                    1,
                    index * 0x0100_0000,
                    1,
                    index * 0x0100_0000,
                    0,
                    0x0100_0000,
                ]),
            ),
        ]
    }

    fn replace(properties: &mut TestProperties, name: &'static str, value: Vec<u8>) {
        properties
            .iter_mut()
            .find(|entry| entry.0 == name)
            .unwrap()
            .1 = value;
    }

    fn dt_with_root(bridges: &[TestProperties], root_properties: TestProperties) -> Vec<u8> {
        let mut nodes = hardware(1, 1, 0);
        for (index, props) in bridges.iter().enumerate() {
            nodes.push(node(alloc::format!("pcie@{index:x}"), props.clone()));
        }
        tree(&nodes, root_properties)
    }

    fn tree(nodes: &[TestNode], root_properties: TestProperties) -> Vec<u8> {
        let mut bytes = vec![0; MAX_DT_SIZE];
        let mut builder = fdt::builder::Builder::new(fdt::builder::BuilderConfig {
            blob_buffer: &mut bytes,
            string_table_cap: 8192,
            memory_reservations: &[],
        })
        .unwrap()
        .start_node("")
        .unwrap();
        let mut names = std::collections::BTreeMap::new();
        for (name, _) in root_properties.iter().chain(nodes.iter().flat_map(|node| {
            node.properties
                .iter()
                .chain(node.children.iter().flat_map(|child| &child.properties))
        })) {
            if !names.contains_key(name) {
                names.insert(*name, builder.add_string(name).unwrap());
            }
        }
        for (name, data) in &root_properties {
            builder = builder.add_prop_array(names[name], &[data]).unwrap();
        }
        builder = emit(builder, nodes, &names);
        let length = builder.end_node().unwrap().build(0).unwrap();
        bytes.truncate(length);
        bytes
    }

    fn dt(bridges: &[TestProperties]) -> Vec<u8> {
        dt_with_root(bridges, root_properties())
    }

    fn fix_checksum(table: &mut [u8]) {
        table[9] = 0;
        table[9] = 0u8.wrapping_sub(checksum(table));
    }

    fn build(dt: &[u8]) -> Result<(Vec<u8>, Vec<u8>), Error<'_>> {
        build_acpi(dt, 1, OUTPUT, CAPACITY, RAM_END, C_BIT)
    }

    fn root_addresses(rsdp: &[u8], tables: &[u8], gpa: u64) -> Vec<u64> {
        assert_eq!(checksum(&rsdp[..20]), 0);
        assert_eq!(checksum(rsdp), 0);
        let xsdt = base_table(tables, gpa, le64(&rsdp[24..])).unwrap();
        assert_eq!(&xsdt[..4], b"XSDT");
        xsdt[HEADER_SIZE..].chunks_exact(8).map(le64).collect()
    }

    fn has(bytes: &[u8], expected: &[u8]) -> bool {
        bytes
            .windows(expected.len())
            .any(|window| window == expected)
    }

    #[test]
    fn preflight_validates_without_a_heap() {
        let dt = dt(&(0..MAX_BRIDGES as u32).map(bridge).collect::<Vec<_>>());
        let (result, bytes) = crate::heap::allocation_measure::measure(|| {
            validate_device_tree(&dt, 1, RAM_END, C_BIT)
        });
        result.unwrap();
        assert_eq!(bytes, 0);

        let (result, bytes) = crate::heap::allocation_measure::measure(|| {
            validate_device_tree(&dt, 1, 0x8000_1000, C_BIT)
        });
        assert!(matches!(result, Err(Error::Overlap)));
        assert_eq!(bytes, 0);
    }

    #[test]
    fn cumulative_allocations_fit_the_measured_heap() {
        let mut bridges = (0..MAX_BRIDGES as u32).map(bridge).collect::<Vec<_>>();
        for (index, properties) in bridges.iter_mut().enumerate() {
            replace(
                properties,
                "linux,pci-domain",
                cells(&[u16::MAX as u32 - index as u32]),
            );
            replace(properties, "bus-range", cells(&[240, 255]));
        }
        let mut nodes = hardware(255, 32, 15);
        for (index, properties) in bridges.into_iter().enumerate() {
            nodes.push(node(alloc::format!("pcie@{index:x}"), properties));
        }
        nodes.extend(metadata());
        let dt = tree(&nodes, root_properties());
        let (preflight, allocated) = crate::heap::allocation_measure::measure(|| {
            validate_device_tree(&dt, 255, RAM_END, C_BIT)
        });
        preflight.unwrap();
        assert_eq!(allocated, 0);
        let (result, bytes) = crate::heap::allocation_measure::measure(|| {
            build_acpi(&dt, 255, OUTPUT, CAPACITY, RAM_END, C_BIT)
        });
        let (_, tables) = result.unwrap();
        assert!(tables.len() <= CAPACITY);
        println!(
            "maximum topology DT: {} bytes; ACPI: {} bytes",
            dt.len(),
            tables.len()
        );
        assert!(build_acpi(&dt, 255, OUTPUT, tables.len(), RAM_END, C_BIT).is_ok());
        assert!(matches!(
            build_acpi(&dt, 255, OUTPUT, tables.len() - 1, RAM_END, C_BIT),
            Err(Error::Capacity)
        ));
        println!("cumulative allocation upper bound, including alignment: {bytes} bytes");
        assert!(bytes <= loader_defs::linux::SNP_BOOT_SHIM_HEAP_SIZE as usize);
    }

    #[test]
    fn zero_bridges_generate_complete_tables() {
        let dt = dt(&[]);
        let (rsdp, tables) = build(&dt).unwrap();
        assert_eq!(root_addresses(&rsdp, &tables, OUTPUT).len(), 3);
        assert_eq!(&tables[..4], b"DSDT");
        validate_output(&tables, OUTPUT, &rsdp, 0).unwrap();
    }

    #[test]
    fn one_and_eight_bridges_have_valid_roots_and_resources() {
        for count in [1, MAX_BRIDGES] {
            let dt = dt(&(0..count as u32).map(bridge).collect::<Vec<_>>());
            let (rsdp, tables) = build(&dt).unwrap();
            let addresses = root_addresses(&rsdp, &tables, OUTPUT);
            assert_eq!(addresses.len(), 5);
            let mcfg = base_table(&tables, OUTPUT, addresses[3]).unwrap();
            let mut segment_count = 0;
            acpi_spec::mcfg::parse_mcfg(mcfg, |segment| {
                assert_eq!(segment.segment.get(), segment_count as u16);
                assert_eq!(
                    segment.ecam_base.get(),
                    0x4000_0000 + segment_count * 0x0100_0000
                );
                assert_eq!(segment.start_bus, 0);
                assert_eq!(segment.end_bus, 15);
                segment_count += 1;
            })
            .unwrap();
            assert_eq!(segment_count, count as u64);
            let ssdt = base_table(&tables, OUTPUT, addresses[4]).unwrap();
            assert_eq!(&ssdt[..4], b"SSDT");
            assert_eq!(
                ssdt.windows(6)
                    .filter(|bytes| *bytes == b"\x08_PXM\x00")
                    .count(),
                count
            );
            assert!(has(ssdt, b"VMOD"));
            assert!(has(ssdt, &acpi_core::ssdt::EisaId(*b"PNP0C02").to_bytes()));
            for index in 0..count {
                assert!(has(ssdt, &[b'P', b'C', b'I', b'0' + index as u8]));
                for base in [
                    0x4000_0000 + index as u64 * 0x0100_0000,
                    0x8000_0000 + index as u64 * 0x0100_0000,
                    FOUR_GB + index as u64 * 0x0100_0000,
                ] {
                    let resource = acpi_core::ssdt::QwordMemory::new(base, 0x0100_0000);
                    let mut resource_bytes = Vec::new();
                    acpi_core::ssdt::ResourceObject::append_to_vec(&resource, &mut resource_bytes);
                    assert!(has(ssdt, &resource_bytes));
                }
            }
            assert!(tables.len() < CAPACITY);
        }
    }

    #[test]
    fn nonzero_start_bus_and_large_segment_use_local_aml_name() {
        let mut properties = bridge(0);
        replace(&mut properties, "bus-range", cells(&[32, 47]));
        replace(&mut properties, "linux,pci-domain", cells(&[65535]));
        let (rsdp, tables) = build(&dt(&[properties])).unwrap();
        let addresses = root_addresses(&rsdp, &tables, OUTPUT);
        let mcfg = base_table(&tables, OUTPUT, addresses[3]).unwrap();
        acpi_spec::mcfg::parse_mcfg(mcfg, |segment| {
            assert_eq!(segment.ecam_base.get(), 0x3e00_0000);
            assert_eq!(segment.start_bus, 32);
            assert_eq!(segment.end_bus, 47);
            assert_eq!(segment.segment.get(), u16::MAX);
        })
        .unwrap();
        let ssdt = base_table(&tables, OUTPUT, addresses[4]).unwrap();
        assert!(has(ssdt, b"PCI0"));
        assert!(has(ssdt, b"\x08_BBN\x0a\x20"));
    }

    #[test]
    fn absent_and_empty_high_windows_do_not_emit_empty_resources() {
        for empty in [false, true] {
            let mut properties = bridge(0);
            let ranges = &mut properties
                .iter_mut()
                .find(|entry| entry.0 == "ranges")
                .unwrap()
                .1;
            if empty {
                ranges[48..56].fill(0);
            } else {
                ranges.truncate(28);
            }
            let (rsdp, tables) = build(&dt(&[properties])).unwrap();
            let addresses = root_addresses(&rsdp, &tables, OUTPUT);
            let ssdt = base_table(&tables, OUTPUT, addresses[4]).unwrap();
            assert_eq!(
                ssdt.windows(3)
                    .filter(|bytes| *bytes == [0x8a, 0x2b, 0])
                    .count(),
                2
            );
        }
    }

    #[test]
    fn malformed_cells_lengths_and_duplicates_fail() {
        for (name, value) in [
            ("#address-cells", cells(&[2])),
            ("#size-cells", cells(&[1])),
            ("bus-range", cells(&[0, 256])),
            ("bus-range", cells(&[16, 15])),
            ("bus-range", cells(&[0])),
            ("linux,pci-domain", cells(&[65536])),
            ("reg", cells(&[0, 0x4000_0000])),
            ("device_type", b"pci".to_vec()),
            ("numa-node-id", vec![]),
            ("ranges", vec![0; 29]),
            ("ranges", vec![]),
        ] {
            let mut properties = bridge(0);
            replace(&mut properties, name, value);
            assert!(build(&dt(&[properties])).is_err(), "{name}");
        }
        let mut properties = bridge(0);
        properties.push(("bus-range", cells(&[0, 15])));
        assert!(matches!(
            build(&dt(&[properties])),
            Err(Error::DuplicateProperty("bus-range"))
        ));
        assert!(matches!(
            build(&dt_with_root(
                &[],
                vec![
                    ("#address-cells", cells(&[1])),
                    ("#size-cells", cells(&[2]))
                ]
            )),
            Err(Error::Property(_))
        ));
        assert!(matches!(
            build(&dt_with_root(
                &[],
                vec![
                    ("#address-cells", cells(&[2])),
                    ("#address-cells", cells(&[2])),
                    ("#size-cells", cells(&[2])),
                ]
            )),
            Err(Error::DuplicateProperty(_))
        ));
    }

    #[test]
    fn unsupported_native_apic_policy_and_cxl_fail() {
        for name in [
            "iommu-map",
            "iommus",
            "msi-map",
            "msi-parent",
            "interrupt-map",
            "interrupt-parent",
            "dma-ranges",
            "cxl-host",
        ] {
            let mut properties = bridge(0);
            properties.push((name, vec![]));
            assert!(
                matches!(build(&dt(&[properties])), Err(Error::Unsupported(_))),
                "{name}"
            );
        }
        for (name, value) in [
            ("numa-node-id", cells(&[1])),
            (
                "compatible",
                b"vendor,other\0pci-host-ecam-generic\0".to_vec(),
            ),
            ("compatible", b"cxl-host\0".to_vec()),
        ] {
            let mut properties = bridge(0);
            replace(&mut properties, name, value);
            assert!(
                matches!(build(&dt(&[properties])), Err(Error::Unsupported(_))),
                "{name}"
            );
        }
        for probe_only in [vec![], cells(&[1])] {
            let mut properties = bridge(0);
            properties.push(("linux,pci-probe-only", probe_only));
            assert!(build(&dt(&[properties])).is_err());
        }
    }

    #[test]
    fn ranges_reject_flags_translations_empty_low_and_duplicate_kinds() {
        for (cell_index, value) in [
            (0, 0x4200_0000),
            (0, 0x0100_0000),
            (4, 0x9000_0000),
            (6, 0),
            (7, 0x0200_0000),
        ] {
            let mut properties = bridge(0);
            let ranges = &mut properties
                .iter_mut()
                .find(|entry| entry.0 == "ranges")
                .unwrap()
                .1;
            ranges[cell_index * 4..cell_index * 4 + 4].copy_from_slice(&u32::to_be_bytes(value));
            assert!(build(&dt(&[properties])).is_err(), "cell {cell_index}");
        }
    }

    #[test]
    fn overlaps_encryption_and_invalid_ecam_fail() {
        for (base, size) in [
            (RAM_END - BUS_SIZE, 16 * BUS_SIZE),
            (ARCH_MMIO_START, 16 * BUS_SIZE),
            (0x8000_0000, 16 * BUS_SIZE),
            (0x4000_0001, 16 * BUS_SIZE),
            (0x4000_0000, BUS_SIZE),
            (C_BIT, 16 * BUS_SIZE),
            (u64::MAX - 4095, 16 * BUS_SIZE),
        ] {
            let mut properties = bridge(0);
            replace(
                &mut properties,
                "reg",
                [base.to_be_bytes(), size.to_be_bytes()].concat(),
            );
            assert!(build(&dt(&[properties])).is_err(), "{base:#x}");
        }
        let mut other = bridge(1);
        replace(&mut other, "reg", cells(&[0, 0x4000_0000, 0, 0x0100_0000]));
        assert!(matches!(
            build(&dt(&[bridge(0), other])),
            Err(Error::Overlap)
        ));
        assert!(matches!(
            build(&dt(&[bridge(0), bridge(0)])),
            Err(Error::DuplicateSegment)
        ));
        assert!(matches!(
            build(&dt(&(0..9).map(bridge).collect::<Vec<_>>())),
            Err(Error::TooManyBridges)
        ));
    }

    #[test]
    fn capacity_is_checked_exactly() {
        for count in [0, 1, 8] {
            let dt = dt(&(0..count).map(bridge).collect::<Vec<_>>());
            let (_, tables) = build(&dt).unwrap();
            assert!(build_acpi(&dt, 1, OUTPUT, tables.len(), RAM_END, C_BIT,).is_ok());
            assert!(matches!(
                build_acpi(&dt, 1, OUTPUT, tables.len() - 1, RAM_END, C_BIT,),
                Err(Error::Capacity)
            ));
        }
    }

    #[test]
    fn corrupt_generated_rsdp_and_tables_fail() {
        let dt = dt(&[]);
        let (rsdp, base) = build(&dt).unwrap();
        for index in [0, 8, 15, 20, 24, 32] {
            let mut corrupt = rsdp.clone();
            corrupt[index] ^= 1;
            assert!(validate_output(&base, OUTPUT, &corrupt, 0).is_err());
        }
        for length in [0, 19, 35] {
            assert!(matches!(
                validate_output(&base, OUTPUT, &rsdp[..length], 0),
                Err(Error::Rsdp)
            ));
        }
        let addresses = root_addresses(&rsdp, &base, OUTPUT);
        for address in addresses {
            let mut corrupt = base.clone();
            corrupt[(address - OUTPUT) as usize + 9] ^= 1;
            assert!(matches!(
                validate_output(&corrupt, OUTPUT, &rsdp, 0),
                Err(Error::BaseAcpi)
            ));
        }
        let mut corrupt_dsdt = base.clone();
        corrupt_dsdt[9] ^= 1;
        assert!(matches!(
            validate_output(&corrupt_dsdt, OUTPUT, &rsdp, 0),
            Err(Error::BaseAcpi)
        ));
    }

    #[test]
    fn unsupported_generated_table_signatures_fail() {
        let dt = dt(&[]);
        let (rsdp, tables) = build(&dt).unwrap();
        let fadt = (root_addresses(&rsdp, &tables, OUTPUT)[0] - OUTPUT) as usize;
        let length = le32(&tables[fadt + 4..]) as usize;
        for signature in [b"MCFG", b"SSDT", b"CEDT", b"IORT"] {
            let mut corrupt = tables.clone();
            corrupt[fadt..fadt + 4].copy_from_slice(signature);
            fix_checksum(&mut corrupt[fadt..fadt + length]);
            assert!(matches!(
                validate_output(&corrupt, OUTPUT, &rsdp, 0),
                Err(Error::BaseTopology)
            ));
        }
    }

    #[test]
    fn declared_dt_size_bounds_all_blocks() {
        let mut dt = dt(&[bridge(0)]);
        let declared = dt.len() - 4;
        dt[4..8].copy_from_slice(&(declared as u32).to_be_bytes());
        assert!(build(&dt).is_err());
        for length in [0, 4, 39] {
            assert!(build(&dt[..length]).is_err());
        }
        let mut over = self::dt(&[]);
        over.resize(MAX_DT_SIZE + 4, 0);
        over[4..8].copy_from_slice(&((MAX_DT_SIZE + 4) as u32).to_be_bytes());
        assert!(matches!(build(&over), Err(Error::DtLimit)));
    }

    fn insert_children(dt: &mut Vec<u8>, structure: &[u8]) {
        let old_structure_length = be32(&dt[36..]) as usize;
        let children_offset = dt.len() - 8;
        dt.splice(children_offset..children_offset, structure.iter().copied());
        let length = dt.len() as u32;
        dt[4..8].copy_from_slice(&length.to_be_bytes());
        dt[36..40]
            .copy_from_slice(&((old_structure_length + structure.len()) as u32).to_be_bytes());
    }

    #[test]
    fn node_depth_and_property_limits_are_enforced() {
        let mut too_deep = dt(&[]);
        let mut structure = Vec::new();
        for _ in 0..MAX_DEPTH {
            structure.extend_from_slice(&[0, 0, 0, 1, b'n', 0, 0, 0]);
        }
        for _ in 0..MAX_DEPTH {
            structure.extend_from_slice(&2u32.to_be_bytes());
        }
        insert_children(&mut too_deep, &structure);
        assert!(matches!(build(&too_deep), Err(Error::DtLimit)));

        let mut too_many = dt(&[]);
        let structure = [0, 0, 0, 1, b'n', 0, 0, 0, 0, 0, 0, 2].repeat(MAX_NODES);
        insert_children(&mut too_many, &structure);
        assert!(matches!(build(&too_many), Err(Error::DtLimit)));

        let mut bytes = vec![0; MAX_DT_SIZE];
        let mut builder = fdt::builder::Builder::new(fdt::builder::BuilderConfig {
            blob_buffer: &mut bytes,
            string_table_cap: 8192,
            memory_reservations: &[],
        })
        .unwrap()
        .start_node("")
        .unwrap();
        for index in 0..=MAX_PROPERTIES {
            let name = builder.add_string(&alloc::format!("prop{index}")).unwrap();
            builder = builder.add_null(name).unwrap();
        }
        let length = builder.end_node().unwrap().build(0).unwrap();
        bytes.truncate(length);
        assert!(matches!(build(&bytes), Err(Error::DtLimit)));
    }

    #[test]
    fn nested_pci_under_a_bus_is_rejected() {
        let mut bytes = dt(&[bridge(0)]);
        let offset = be32(&bytes[8..]) as usize;
        // The builder puts root properties before the PCI child's BEGIN_NODE.
        let parser = Parser::new(&bytes).unwrap();
        let root_property_bytes: usize = parser
            .root()
            .unwrap()
            .properties()
            .map(|property| 12 + property.unwrap().data.len().next_multiple_of(4))
            .sum();
        let child_offset = offset + 8 + root_property_bytes;
        bytes.splice(child_offset..child_offset, [0, 0, 0, 1, b'b', 0, 0, 0]);
        let end_offset = bytes.len() - 8;
        bytes.splice(end_offset..end_offset, 2u32.to_be_bytes());
        let length = bytes.len() as u32;
        let structure_length = be32(&bytes[36..]) + 12;
        bytes[4..8].copy_from_slice(&length.to_be_bytes());
        bytes[36..40].copy_from_slice(&structure_length.to_be_bytes());
        assert!(matches!(build(&bytes), Err(Error::Unsupported(_))));
    }

    #[test]
    fn xsdt_lengths_entry_limits_and_dsdt_pointers_are_checked() {
        let dt = dt(&[]);
        let (rsdp, base) = build(&dt).unwrap();
        let xsdt_offset = (le64(&rsdp[24..]) - OUTPUT) as usize;
        for length in [0, 35, 37, u32::MAX] {
            let mut corrupt = base.clone();
            corrupt[xsdt_offset + 4..xsdt_offset + 8].copy_from_slice(&length.to_le_bytes());
            if (HEADER_SIZE..=base.len() - xsdt_offset).contains(&(length as usize)) {
                fix_checksum(&mut corrupt[xsdt_offset..xsdt_offset + length as usize]);
            }
            assert!(matches!(
                validate_output(&corrupt, OUTPUT, &rsdp, 0),
                Err(Error::BaseAcpi)
            ));
        }
        let fadt_offset = (root_addresses(&rsdp, &base, OUTPUT)[0] - OUTPUT) as usize;
        let fadt_length = le32(&base[fadt_offset + 4..]) as usize;
        for pointer in [OUTPUT - 8, OUTPUT + base.len() as u64, u64::MAX] {
            let mut corrupt = base.clone();
            let fadt = &mut corrupt[fadt_offset..fadt_offset + fadt_length];
            fadt[40..44].fill(0);
            fadt[140..148].copy_from_slice(&pointer.to_le_bytes());
            fix_checksum(fadt);
            assert!(matches!(
                validate_output(&corrupt, OUTPUT, &rsdp, 0),
                Err(Error::BaseAcpi)
            ));
        }
        let mut corrupt = base.clone();
        let length = le32(&corrupt[xsdt_offset + 4..]) as usize;
        corrupt[xsdt_offset + HEADER_SIZE..xsdt_offset + HEADER_SIZE + 8]
            .copy_from_slice(&(OUTPUT - 8).to_le_bytes());
        fix_checksum(&mut corrupt[xsdt_offset..xsdt_offset + length]);
        assert!(matches!(
            validate_output(&corrupt, OUTPUT, &rsdp, 0),
            Err(Error::BaseAcpi)
        ));
    }

    #[test]
    fn high_window_encryption_boundary_and_low_window_ram_overlap_fail() {
        for (base, size, flags) in [
            (RAM_END - 4096, 8192, 0x0200_0000u32),
            (ARCH_MMIO_START - 4096, 8192, 0x0200_0000),
            (C_BIT - 4096, 8192, 0x0300_0000),
            (C_BIT, 4096, 0x0300_0000),
        ] {
            let mut properties = bridge(0);
            let mut range = flags.to_be_bytes().to_vec();
            range.extend_from_slice(&base.to_be_bytes());
            range.extend_from_slice(&base.to_be_bytes());
            range.extend_from_slice(&u64::to_be_bytes(size));
            if flags == 0x0300_0000 {
                let original = properties.iter().find(|entry| entry.0 == "ranges").unwrap();
                range.splice(..0, original.1[..28].iter().copied());
            }
            replace(&mut properties, "ranges", range);
            assert!(build(&dt(&[properties])).is_err());
        }
    }

    #[test]
    fn invalid_output_and_address_limits_fail() {
        let dt = dt(&[]);
        for output in [0, OUTPUT + 1, RAM_END, u64::MAX - 7] {
            assert!(build_acpi(&dt, 1, output, CAPACITY, RAM_END, C_BIT).is_err());
        }
        for c_bit in [0, C_BIT + 1, 1 << 52] {
            assert!(matches!(
                build_acpi(&dt, 1, OUTPUT, CAPACITY, RAM_END, c_bit),
                Err(Error::AddressLimit)
            ));
        }
    }

    fn rejected(nodes: &[TestNode], expected: u32) {
        let dt = tree(nodes, root_properties());
        let (result, bytes) = crate::heap::allocation_measure::measure(|| {
            validate_device_tree(&dt, expected, RAM_END, C_BIT)
        });
        assert!(result.is_err(), "accepted malformed topology");
        assert_eq!(bytes, 0);
        assert!(build_acpi(&dt, expected, OUTPUT, CAPACITY, RAM_END, C_BIT).is_err());
    }

    #[test]
    fn cpu_identity_status_and_measured_count_are_required() {
        for (count, expected) in [(0, 1), (1, 0), (1, 2), (2, 1), (256, 256), (255, 256)] {
            rejected(&hardware(count, 1, 0), expected);
        }
        for (property, value) in [
            ("reg", cells(&[255])),
            ("reg", cells(&[0, 0])),
            ("numa-node-id", cells(&[1])),
            ("status", b"disabled\0".to_vec()),
            ("device_type", b"other\0".to_vec()),
        ] {
            let mut nodes = hardware(1, 1, 0);
            replace(&mut nodes[0].children[0].properties, property, value);
            rejected(&nodes, 1);
        }
        let mut nodes = hardware(2, 1, 0);
        replace(&mut nodes[0].children[1].properties, "reg", cells(&[0]));
        rejected(&nodes, 2);
        let mut nodes = hardware(2, 1, 0);
        nodes[0].children[1].name = "cpu@01".into();
        rejected(&nodes, 2);
        let mut nodes = hardware(1, 1, 0);
        nodes[0].children[0].properties.push(("reg", cells(&[0])));
        rejected(&nodes, 1);
        let mut dt = tree(&hardware(1, 1, 0), root_properties());
        dt[28..32].copy_from_slice(&1u32.to_be_bytes());
        assert!(validate_device_tree(&dt, 1, RAM_END, C_BIT).is_err());
    }

    #[test]
    fn memory_records_must_normalize_to_measured_ram() {
        let mut nodes = hardware(1, 32, 0);
        nodes[1..].reverse();
        let dt = tree(&nodes, root_properties());
        validate_device_tree(&dt, 1, RAM_END, C_BIT).unwrap();
        rejected(&hardware(1, 33, 0), 1);
        rejected(&hardware(1, 0, 0), 1);
        for (start, len) in [
            (0, 0),
            (0, RAM_END - 4096),
            (0, RAM_END + 4096),
            (4096, RAM_END - 4096),
            (0, RAM_END - 1),
            (u64::MAX - 4095, 4096),
        ] {
            let mut nodes = hardware(1, 1, 0);
            nodes[1].name = alloc::format!("memory@{start:x}");
            replace(
                &mut nodes[1].properties,
                "reg",
                [start.to_be_bytes(), len.to_be_bytes()].concat(),
            );
            rejected(&nodes, 1);
        }
        for (property, value) in [
            ("numa-node-id", cells(&[1])),
            (
                igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY,
                cells(&[u32::MAX]),
            ),
            (
                igvm_defs::dt::IGVM_DT_IGVM_TYPE_PROPERTY,
                cells(&[u32::from(igvm_defs::MemoryMapEntryType::VTL2_PROTECTABLE.0)]),
            ),
            ("reg", vec![0; 8]),
        ] {
            let mut nodes = hardware(1, 1, 0);
            replace(&mut nodes[1].properties, property, value);
            rejected(&nodes, 1);
        }
        let mut nodes = hardware(1, 2, 0);
        nodes[2].name = "memory@0".into();
        replace(
            &mut nodes[2].properties,
            "reg",
            [0u64.to_be_bytes(), RAM_END.to_be_bytes()].concat(),
        );
        rejected(&nodes, 1);
    }

    #[test]
    fn uart_inventory_is_not_inferred_from_console_metadata() {
        for mask in [0, 3, 7, 11, 15] {
            let mut nodes = hardware(1, 1, mask);
            nodes.push(node(
                "chosen".into(),
                vec![
                    ("bootargs", b"ignored\0".to_vec()),
                    ("stdout-path", b"/pio-bus/serial@3e8\0".to_vec()),
                ],
            ));
            let (rsdp, tables) = build(&tree(&nodes, root_properties())).unwrap();
            let dsdt = base_table(&tables, OUTPUT, OUTPUT).unwrap();
            for index in 0..4 {
                let name = [b'U', b'A', b'R', b'1' + index];
                assert_eq!(has(dsdt, &name), mask & (1 << index) != 0);
            }
            validate_output(&tables, OUTPUT, &rsdp, 0).unwrap();
        }
        for (property, value) in [
            ("compatible", b"arm,pl011\0".to_vec()),
            ("reg", [0x3f8u64.to_be_bytes(), 9u64.to_be_bytes()].concat()),
            ("reg", cells(&[0x3f8, 8])),
            ("interrupts", 3u64.to_be_bytes().to_vec()),
            ("interrupts", cells(&[4])),
            ("current-speed", cells(&[0])),
        ] {
            let mut nodes = hardware(1, 1, 3);
            replace(&mut nodes[2].children[0].properties, property, value);
            rejected(&nodes, 1);
        }
        let mut nodes = hardware(1, 1, 15);
        let mut duplicate = nodes[2].children[0].clone();
        duplicate.name = "serial@03f8".into();
        nodes[2].children.push(duplicate);
        rejected(&nodes, 1);
        let mut nodes = hardware(1, 1, 3);
        nodes[2].children[1] = nodes[2].children[0].clone();
        nodes[2].children[1].name = "serial@03f8".into();
        rejected(&nodes, 1);
    }

    fn metadata() -> Vec<TestNode> {
        let mut bus = node(
            "bus".into(),
            vec![
                ("compatible", b"simple-bus\0".to_vec()),
                ("#address-cells", cells(&[2])),
                ("#size-cells", cells(&[2])),
                ("ranges", vec![]),
            ],
        );
        for vtl in [0, 2] {
            bus.children.push(node(
                alloc::format!("vmbus-vtl{vtl}"),
                vec![
                    ("compatible", b"microsoft,vmbus\0".to_vec()),
                    ("#address-cells", cells(&[2])),
                    ("#size-cells", cells(&[2])),
                    (igvm_defs::dt::IGVM_DT_VTL_PROPERTY, cells(&[vtl])),
                    (
                        "microsoft,message-connection-id",
                        cells(&[if vtl == 0 { 1 } else { 4 }]),
                    ),
                    ("ranges", vec![]),
                ],
            ));
        }
        let mut openhcl = node(
            "openhcl".into(),
            vec![("memory-allocation-mode", b"host\0".to_vec())],
        );
        openhcl
            .children
            .push(node("entropy".into(), vec![("reg", vec![42; 256])]));
        openhcl.children.push(node(
            "keep-alive".into(),
            vec![("device-types", b"nvme\0".to_vec())],
        ));
        vec![
            bus,
            openhcl,
            node(
                "chosen".into(),
                vec![("bootargs", b"unmeasured\0".to_vec())],
            ),
        ]
    }

    #[test]
    fn known_partition_metadata_is_not_guest_hardware() {
        let mut nodes = hardware(1, 1, 0);
        nodes.extend(metadata());
        replace(
            &mut nodes[2].children[0].properties,
            "ranges",
            [
                0xe000_0000u64,
                0xe000_0000,
                0x1000_0000,
                FOUR_GB,
                FOUR_GB,
                0x1000_0000,
            ]
            .into_iter()
            .flat_map(u64::to_be_bytes)
            .collect(),
        );
        let (_, tables) = build(&tree(&nodes, root_properties())).unwrap();
        assert!(!has(&tables, b"VMBUS"));
        assert!(!has(&tables, b"unmeasured"));
        let mut bad = nodes.clone();
        bad.push(node(
            "watchdog".into(),
            vec![("compatible", b"watchdog\0".to_vec())],
        ));
        rejected(&bad, 1);
        let mut bad = nodes.clone();
        bad[2].children.push(node("mystery".into(), vec![]));
        rejected(&bad, 1);
        let mut bad = nodes.clone();
        bad[3].children.push(node("mystery".into(), vec![]));
        rejected(&bad, 1);
        let mut bad = nodes.clone();
        replace(
            &mut bad[3].properties,
            "memory-allocation-mode",
            b"vtl2\0".to_vec(),
        );
        rejected(&bad, 1);
        let mut bad = nodes.clone();
        replace(
            &mut bad[2].children[1].properties,
            igvm_defs::dt::IGVM_DT_VTL_PROPERTY,
            cells(&[0]),
        );
        rejected(&bad, 1);
        for values in [
            [RAM_END - 4096, RAM_END - 4096, 8192],
            [FOUR_GB, FOUR_GB + 4096, 4096],
            [u64::MAX - 4095, u64::MAX - 4095, 4096],
        ] {
            let mut bad = nodes.clone();
            replace(
                &mut bad[2].children[0].properties,
                "ranges",
                values.into_iter().flat_map(u64::to_be_bytes).collect(),
            );
            rejected(&bad, 1);
        }
        let mut bad = nodes.clone();
        replace(
            &mut bad[2].children[1].properties,
            "microsoft,message-connection-id",
            cells(&[0x800074]),
        );
        rejected(&bad, 1);
        let mut bad = nodes.clone();
        replace(
            &mut bad[4].properties,
            "bootargs",
            b"bad\0suffix\0".to_vec(),
        );
        rejected(&bad, 1);
        let mut bad = nodes.clone();
        bad[4]
            .properties
            .push(("bootargs", b"duplicate\0".to_vec()));
        rejected(&bad, 1);
        let mut bad = nodes.clone();
        bad.push(nodes[4].clone());
        rejected(&bad, 1);
        let mut bad = nodes;
        bad[1].properties.push(("mystery", vec![]));
        rejected(&bad, 1);
    }

    #[test]
    fn table_cpu_identities_and_affinities_match_dt() {
        let dt = tree(&hardware(255, 32, 15), root_properties());
        let (rsdp, tables) = build_acpi(&dt, 255, OUTPUT, CAPACITY, RAM_END, C_BIT).unwrap();
        let addresses = root_addresses(&rsdp, &tables, OUTPUT);
        let fadt = base_table(&tables, OUTPUT, addresses[0]).unwrap();
        assert_eq!(le64(&fadt[140..]), OUTPUT);
        let madt = base_table(&tables, OUTPUT, addresses[1]).unwrap();
        let srat = base_table(&tables, OUTPUT, addresses[2]).unwrap();
        let mut apics = 0;
        let mut entries = &madt[HEADER_SIZE + 8..];
        while !entries.is_empty() {
            let length = entries[1] as usize;
            if entries[0] == 0 {
                assert_eq!(entries[2], apics + 1);
                assert_eq!(entries[3], apics);
                assert_eq!(le32(&entries[4..]), 1);
                apics += 1;
            }
            entries = &entries[length..];
        }
        assert_eq!(apics, 255);
        let mut apics = 0;
        let mut memories = 0;
        let mut entries = &srat[HEADER_SIZE + 12..];
        while !entries.is_empty() {
            let length = entries[1] as usize;
            if entries[0] == 0 {
                assert_eq!(entries[2], 0);
                assert_eq!(entries[3], apics);
                apics += 1;
            } else if entries[0] == 1 {
                assert_eq!(le32(&entries[2..]), 0);
                assert_eq!(le64(&entries[8..]), 0);
                assert_eq!(le64(&entries[16..]), RAM_END);
                memories += 1;
            }
            entries = &entries[length..];
        }
        assert_eq!((apics, memories), (255, 1));
    }
}
