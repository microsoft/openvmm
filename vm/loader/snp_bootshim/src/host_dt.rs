// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reads the CPUs, RAM, PCIe host bridges, and COM ports from the host
//! device tree.
//!
//! The device tree is unmeasured host input. The shim trusts the RAM that it
//! describes. The shim reads only what it needs and ignores all other nodes
//! and properties.

use acpi::minimal::Cpu;
use acpi::minimal::Ram;
use arrayvec::ArrayVec;
use fdt::parser::Node;
use fdt::parser::Parser;
use fdt::parser::Property;
use loader_defs::linux::SNP_BOOT_SHIM_MAX_CPUS;
use loader_defs::linux::SNP_BOOT_SHIM_MAX_PCIE_BRIDGES;
use loader_defs::linux::SNP_BOOT_SHIM_MAX_RAM_END;
use loader_defs::linux::SNP_BOOT_SHIM_MAX_RAM_RANGES;
use memory_range::MemoryRange;

/// The size of one PCIe bus in ECAM space.
pub(crate) const BUS_SIZE: u64 = 1 << 20;
const PCI_SPACE_MEM32: u32 = 0x0200_0000;
const PCI_SPACE_MEM64: u32 = 0x0300_0000;
/// The size of one `ranges` entry: 3 PCI, 2 CPU, and 2 size cells.
const PCI_RANGE_SIZE: usize = 28;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error<'a> {
    // A borrowed parser error cannot implement Error::source's 'static contract.
    #[error("invalid device tree: {0}")]
    Fdt(fdt::parser::Error<'a>),
    #[error("missing or invalid device-tree property {0}")]
    Property(&'static str),
    #[error("more than {SNP_BOOT_SHIM_MAX_CPUS} CPUs")]
    TooManyCpus,
    #[error("more than {SNP_BOOT_SHIM_MAX_RAM_RANGES} RAM ranges")]
    TooManyRamRanges,
    #[error("more than {SNP_BOOT_SHIM_MAX_PCIE_BRIDGES} PCIe host bridges")]
    TooManyBridges,
    #[error("two PCIe host bridges share a segment")]
    DuplicateSegment,
    #[error("a RAM range is unaligned or reaches the C-bit or the shim's mapping limit")]
    Ram,
    #[error("a PCIe ECAM or MMIO window is invalid, overlaps RAM, or reaches the C-bit")]
    Window,
}

impl<'a> From<fdt::parser::Error<'a>> for Error<'a> {
    fn from(error: fdt::parser::Error<'a>) -> Self {
        Self::Fdt(error)
    }
}

/// A PCIe host bridge with identity-mapped windows.
#[derive(Debug, Copy, Clone)]
pub(crate) struct Bridge {
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
    pub ecam: MemoryRange,
    pub low_mmio: MemoryRange,
    pub high_mmio: MemoryRange,
}

/// The host devices that the shim accepts and describes in ACPI.
#[derive(Debug, Default)]
pub(crate) struct HostDevices {
    /// The CPUs, in processor-index order.
    pub cpus: ArrayVec<Cpu, SNP_BOOT_SHIM_MAX_CPUS>,
    pub ram: ArrayVec<Ram, SNP_BOOT_SHIM_MAX_RAM_RANGES>,
    pub bridges: ArrayVec<Bridge, SNP_BOOT_SHIM_MAX_PCIE_BRIDGES>,
    /// Which of COM1 through COM4 are present.
    pub com_ports: [bool; 4],
}

/// Reads the host devices from `dt`.
///
/// `measured_ram_end` is the end of the RAM that the image measures. RAM must
/// lie below the C-bit and the shim's mapping limit, which the shim needs to
/// accept it.
///
/// Every PCIe window must lie outside RAM and below the C-bit. A window over
/// private RAM, or one that the C-bit would encrypt, does not fail when Linux
/// assigns it, but device accesses then reach the wrong memory.
pub(crate) fn read(
    dt: &[u8],
    measured_ram_end: u64,
    c_bit_mask: u64,
) -> Result<HostDevices, Error<'_>> {
    let parser = Parser::new(dt)?;
    let mut devices = HostDevices::default();
    for node in parser.root()?.children() {
        let node = node?;
        if node.name == "cpus" {
            for cpu in node.children() {
                let cpu = cpu?;
                devices
                    .cpus
                    .try_push(Cpu {
                        id: property(&cpu, "reg")?.read_u32(0)?,
                        vnode: vnode(&cpu)?,
                    })
                    .map_err(|_| Error::TooManyCpus)?;
            }
        } else if node.name.starts_with("memory@") {
            let reg = property(&node, "reg")?;
            let (start, size) = (reg.read_u64(0)?, reg.read_u64(1)?);
            let range = start
                .checked_add(size)
                .and_then(|end| MemoryRange::try_new(start..end).ok())
                .filter(|r| r.end() <= SNP_BOOT_SHIM_MAX_RAM_END.min(c_bit_mask))
                .ok_or(Error::Ram)?;
            devices
                .ram
                .try_push(Ram {
                    range,
                    vnode: vnode(&node)?,
                })
                .map_err(|_| Error::TooManyRamRanges)?;
        } else if node.name == "pio-bus" {
            read_com_ports(&node, &mut devices.com_ports)?;
        } else if is_pcie_bridge(&node)? {
            let bridge = read_bridge(&node, c_bit_mask)?;
            if devices.bridges.iter().any(|b| b.segment == bridge.segment) {
                return Err(Error::DuplicateSegment);
            }
            devices
                .bridges
                .try_push(bridge)
                .map_err(|_| Error::TooManyBridges)?;
        }
    }
    let measured_ram = MemoryRange::new(0..measured_ram_end);
    let overlaps_ram = |window: &MemoryRange| {
        window.overlaps(&measured_ram) || devices.ram.iter().any(|r| window.overlaps(&r.range))
    };
    if devices
        .bridges
        .iter()
        .flat_map(|b| [b.ecam, b.low_mmio, b.high_mmio])
        .any(|w| overlaps_ram(&w))
    {
        return Err(Error::Window);
    }
    Ok(devices)
}

fn vnode<'a>(node: &Node<'a>) -> Result<u32, Error<'a>> {
    match node.find_property("numa-node-id")? {
        Some(p) => Ok(p.read_u32(0)?),
        None => Ok(0),
    }
}

fn property<'a>(node: &Node<'a>, name: &'static str) -> Result<Property<'a>, Error<'a>> {
    node.find_property(name)?.ok_or(Error::Property(name))
}

fn is_pcie_bridge<'a>(node: &Node<'a>) -> Result<bool, Error<'a>> {
    Ok(node
        .find_property("compatible")?
        .is_some_and(|p| p.data == b"pci-host-ecam-generic\0"))
}

fn read_com_ports<'a>(pio_bus: &Node<'a>, com_ports: &mut [bool; 4]) -> Result<(), Error<'a>> {
    for uart in pio_bus.children() {
        let base = property(&uart?, "reg")?.read_u64(0)?;
        if let Some(index) = x86defs::serial::COM_BASES
            .iter()
            .position(|&com_base| u64::from(com_base) == base)
        {
            com_ports[index] = true;
        }
    }
    Ok(())
}

fn read_bridge<'a>(node: &Node<'a>, c_bit_mask: u64) -> Result<Bridge, Error<'a>> {
    let segment = u16::try_from(property(node, "linux,pci-domain")?.read_u32(0)?)
        .map_err(|_| Error::Property("linux,pci-domain"))?;
    let buses = property(node, "bus-range")?;
    let start_bus = u8::try_from(buses.read_u32(0)?).map_err(|_| Error::Property("bus-range"))?;
    let end_bus = u8::try_from(buses.read_u32(1)?).map_err(|_| Error::Property("bus-range"))?;
    if start_bus > end_bus {
        return Err(Error::Property("bus-range"));
    }
    // MCFG gives Linux the ECAM space of every bus in `bus-range`, so check
    // that span and ignore the size in `reg`.
    let bus_count = u64::from(end_bus - start_bus) + 1;
    let ecam_base = property(node, "reg")?.read_u64(0)?;
    let ecam = window(ecam_base, bus_count * BUS_SIZE, c_bit_mask)?;
    // MCFG describes the ECAM base of bus 0.
    if ecam.start() < u64::from(start_bus) * BUS_SIZE {
        return Err(Error::Window);
    }
    let mut low_mmio = MemoryRange::EMPTY;
    let mut high_mmio = MemoryRange::EMPTY;
    for range in property(node, "ranges")?.data.chunks_exact(PCI_RANGE_SIZE) {
        let slot = match be32(&range[..4]) {
            PCI_SPACE_MEM32 => &mut low_mmio,
            PCI_SPACE_MEM64 => &mut high_mmio,
            _ => continue,
        };
        // The ACPI tables describe identity windows at the CPU address.
        *slot = window(be64(&range[12..]), be64(&range[20..]), c_bit_mask)?;
    }
    Ok(Bridge {
        segment,
        start_bus,
        end_bus,
        ecam,
        low_mmio,
        high_mmio,
    })
}

fn window<'a>(base: u64, size: u64, c_bit_mask: u64) -> Result<MemoryRange, Error<'a>> {
    let end = base.checked_add(size).ok_or(Error::Window)?;
    let range = MemoryRange::try_new(base..end).map_err(|_| Error::Window)?;
    if end > c_bit_mask {
        return Err(Error::Window);
    }
    Ok(range)
}

// The callers pass slices of a fixed-size `ranges` entry.
fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn be64(bytes: &[u8]) -> u64 {
    (u64::from(be32(bytes)) << 32) | u64::from(be32(&bytes[4..]))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use test_with_tracing::test;

    pub(crate) const RAM_END: u64 = 0x1000_0000;
    pub(crate) const C_BIT: u64 = 1 << 47;
    /// RAM above the measured RAM, on NUMA node 1, below the PCIe windows.
    pub(crate) const HIGH_RAM: MemoryRange = MemoryRange::new(0x2000_0000..0x3000_0000);

    type Properties = Vec<(&'static str, Vec<u8>)>;
    type TestNode = (String, Properties, Vec<(String, Properties)>);

    pub(crate) fn cells(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_be_bytes())
            .collect()
    }

    /// Returns the properties of the PCIe bridge that OpenVMM describes for
    /// segment `index`.
    pub(crate) fn bridge(index: u32) -> Properties {
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
                    PCI_SPACE_MEM32,
                    0,
                    0x8000_0000 + index * 0x0100_0000,
                    0,
                    0x8000_0000 + index * 0x0100_0000,
                    0,
                    0x0100_0000,
                    PCI_SPACE_MEM64,
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

    pub(crate) fn replace(properties: &mut Properties, name: &str, value: Vec<u8>) {
        properties
            .iter_mut()
            .find(|entry| entry.0 == name)
            .unwrap()
            .1 = value;
    }

    /// Builds a device tree like the one that OpenVMM gives an SNP image,
    /// with the given COM ports and PCIe bridges.
    pub(crate) fn device_tree(com_ports: [bool; 4], bridges: &[Properties]) -> Vec<u8> {
        let mut nodes: Vec<TestNode> = vec![(
            "cpus".into(),
            vec![
                ("#address-cells", cells(&[1])),
                ("#size-cells", cells(&[0])),
            ],
            vec![
                (
                    "cpu@1".into(),
                    vec![("device_type", b"cpu\0".to_vec()), ("reg", cells(&[0]))],
                ),
                (
                    "cpu@2".into(),
                    vec![
                        ("device_type", b"cpu\0".to_vec()),
                        ("reg", cells(&[5])),
                        ("numa-node-id", cells(&[1])),
                    ],
                ),
            ],
        )];
        for (start, end, vnode) in [(0, RAM_END, 0), (HIGH_RAM.start(), HIGH_RAM.end(), 1)] {
            nodes.push((
                alloc::format!("memory@{start:x}"),
                vec![
                    ("device_type", b"memory\0".to_vec()),
                    (
                        "reg",
                        [start, end - start]
                            .into_iter()
                            .flat_map(u64::to_be_bytes)
                            .collect(),
                    ),
                    ("numa-node-id", cells(&[vnode])),
                ],
                vec![],
            ));
        }
        let uarts = com_ports
            .iter()
            .zip(x86defs::serial::COM_BASES)
            .filter(|(present, _)| **present)
            .map(|(_, base)| {
                let base = u64::from(base);
                (
                    alloc::format!("serial@{base:x}"),
                    vec![
                        ("compatible", b"ns16550\0".to_vec()),
                        ("reg", [base.to_be_bytes(), 8u64.to_be_bytes()].concat()),
                    ],
                )
            })
            .collect();
        nodes.push((
            "pio-bus".into(),
            vec![("compatible", b"x86-pio-bus\0".to_vec())],
            uarts,
        ));
        for (index, properties) in bridges.iter().enumerate() {
            nodes.push((alloc::format!("pcie@{index:x}"), properties.clone(), vec![]));
        }

        let mut bytes = vec![0; loader_defs::linux::SNP_BOOT_SHIM_DT_SIZE as usize];
        let mut builder = fdt::builder::Builder::new(fdt::builder::BuilderConfig {
            blob_buffer: &mut bytes,
            string_table_cap: 1024,
            memory_reservations: &[],
        })
        .unwrap();
        let mut names = std::collections::BTreeMap::new();
        for (name, _) in nodes.iter().flat_map(|(_, properties, children)| {
            properties
                .iter()
                .chain(children.iter().flat_map(|(_, properties)| properties))
        }) {
            if !names.contains_key(name) {
                names.insert(*name, builder.add_string(name).unwrap());
            }
        }
        let mut root = builder.start_node("").unwrap();
        for (name, properties, children) in &nodes {
            let mut node = root.start_node(name).unwrap();
            for (property, data) in properties {
                node = node.add_prop_array(names[property], &[data]).unwrap();
            }
            for (name, properties) in children {
                let mut child = node.start_node(name).unwrap();
                for (property, data) in properties {
                    child = child.add_prop_array(names[property], &[data]).unwrap();
                }
                node = child.end_node().unwrap();
            }
            root = node.end_node().unwrap();
        }
        let length = root.end_node().unwrap().build(0).unwrap();
        bytes.truncate(length);
        bytes
    }

    #[test]
    fn reads_bridges_and_com_ports() {
        let bridges = (0..SNP_BOOT_SHIM_MAX_PCIE_BRIDGES as u32)
            .map(bridge)
            .collect::<Vec<_>>();
        let dt = device_tree([true, false, true, false], &bridges);
        let devices = read(&dt, RAM_END, C_BIT).unwrap();
        assert_eq!(devices.com_ports, [true, false, true, false]);
        let cpus = devices
            .cpus
            .iter()
            .map(|c| (c.id, c.vnode))
            .collect::<Vec<_>>();
        assert_eq!(cpus, [(0, 0), (5, 1)]);
        let ram = devices
            .ram
            .iter()
            .map(|r| (r.range, r.vnode))
            .collect::<Vec<_>>();
        assert_eq!(ram, [(MemoryRange::new(0..RAM_END), 0), (HIGH_RAM, 1)]);
        assert_eq!(devices.bridges.len(), SNP_BOOT_SHIM_MAX_PCIE_BRIDGES);
        for (index, bridge) in devices.bridges.iter().enumerate() {
            let offset = index as u64 * 0x0100_0000;
            assert_eq!(bridge.segment, index as u16);
            assert_eq!((bridge.start_bus, bridge.end_bus), (0, 15));
            assert_eq!(bridge.ecam.start(), 0x4000_0000 + offset);
            assert_eq!(bridge.low_mmio.start(), 0x8000_0000 + offset);
            assert_eq!(bridge.high_mmio.start(), (1 << 32) + offset);
        }
    }

    #[test]
    fn rejects_too_many_or_duplicate_bridges() {
        let bridges = (0..=SNP_BOOT_SHIM_MAX_PCIE_BRIDGES as u32)
            .map(bridge)
            .collect::<Vec<_>>();
        let dt = device_tree([false; 4], &bridges);
        assert!(matches!(
            read(&dt, RAM_END, C_BIT),
            Err(Error::TooManyBridges)
        ));
        let mut duplicate = bridge(1);
        replace(&mut duplicate, "linux,pci-domain", cells(&[0]));
        let dt = device_tree([false; 4], &[bridge(0), duplicate]);
        assert!(matches!(
            read(&dt, RAM_END, C_BIT),
            Err(Error::DuplicateSegment)
        ));
    }

    #[test]
    fn rejects_windows_over_ram_or_at_the_c_bit() {
        let dt = device_tree([false; 4], &[bridge(0)]);
        // The ECAM window starts at 1 GiB, and the high window ends at 4 GiB + 16 MiB.
        for (ram_end, c_bit) in [(0x4000_1000, C_BIT), (RAM_END, 1 << 32)] {
            assert!(matches!(read(&dt, ram_end, c_bit), Err(Error::Window)));
        }
    }

    #[test]
    fn ecam_window_covers_every_bus() {
        // `reg` gives one bus of ECAM space, but `bus-range` has 16 buses
        // that reach the C-bit.
        let mut properties = bridge(0);
        let base = C_BIT - BUS_SIZE;
        replace(
            &mut properties,
            "reg",
            [base, BUS_SIZE]
                .into_iter()
                .flat_map(u64::to_be_bytes)
                .collect(),
        );
        let dt = device_tree([false; 4], &[properties.clone()]);
        assert!(matches!(read(&dt, RAM_END, C_BIT), Err(Error::Window)));

        // An empty `reg` size still gives a 16-bus ECAM window.
        replace(&mut properties, "reg", cells(&[0, 0x4000_0000, 0, 0]));
        let dt = device_tree([false; 4], &[properties]);
        let devices = read(&dt, RAM_END, C_BIT).unwrap();
        let ecam = devices.bridges[0].ecam;
        assert_eq!(ecam, MemoryRange::new(0x4000_0000..0x4100_0000));
    }

    #[test]
    fn rejects_invalid_bus_ranges() {
        for buses in [[16, 15], [0, 256]] {
            let mut properties = bridge(0);
            replace(&mut properties, "bus-range", cells(&buses));
            let dt = device_tree([false; 4], &[properties]);
            assert!(matches!(
                read(&dt, RAM_END, C_BIT),
                Err(Error::Property("bus-range"))
            ));
        }
        // ECAM for bus 0 would start below address zero.
        let mut properties = bridge(0);
        replace(
            &mut properties,
            "reg",
            cells(&[0, 0x0200_0000, 0, 0x0010_0000]),
        );
        replace(&mut properties, "bus-range", cells(&[255, 255]));
        let dt = device_tree([false; 4], &[properties]);
        assert!(matches!(read(&dt, 0x0100_0000, C_BIT), Err(Error::Window)));
    }
}
