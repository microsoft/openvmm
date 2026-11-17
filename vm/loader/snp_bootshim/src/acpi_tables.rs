// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds the complete ACPI table set that the shim gives to Linux.
//!
//! The CPUs, RAM, PCIe host bridges, and COM ports come from the host device
//! tree.

use crate::host_dt::BUS_SIZE;
use crate::host_dt::HostDevices;
use acpi::builder::Builder;
use acpi::builder::OemInfo;
use acpi::builder::Table;
use acpi::minimal::Arch;
use acpi::minimal::Platform;
use acpi::minimal::X86;
use acpi::ssdt::PcieHostBridgeEntry;
use acpi::ssdt::Ssdt;
use acpi_spec::mcfg::McfgHeader;
use acpi_spec::mcfg::McfgSegmentBusRange;
use alloc::vec::Vec;
use zerocopy::IntoBytes;

const OEM: OemInfo = OemInfo {
    oem_id: *b"HVLITE",
    oem_tableid: *b"HVLITETB",
    oem_revision: 0,
    creator_id: *b"MSHV",
    creator_revision: 0,
};

/// Returns the RSDP bytes and the table bytes, as `Builder::build` does.
///
/// Every pointer in the tables and the RSDP refers to `output_gpa`, where the
/// caller must copy the table bytes.
pub(crate) fn build(devices: &HostDevices, output_gpa: u64) -> (Vec<u8>, Vec<u8>) {
    let base = Platform {
        cpus: &devices.cpus,
        ram: &devices.ram,
        arch: Arch::X86(X86 {
            com_ports: devices.com_ports,
        }),
    }
    .build(&OEM);
    let mut builder = Builder::new(output_gpa, OEM);
    let dsdt = builder.append_raw(&base.dsdt);
    builder.append_raw(&base.fadt(dsdt, &OEM));
    builder.append_raw(&base.madt);
    builder.append_raw(&base.srat);

    if !devices.bridges.is_empty() {
        let mut ssdt = Ssdt::new();
        let mut segments = Vec::new();
        for (index, bridge) in devices.bridges.iter().enumerate() {
            ssdt.add_pcie(PcieHostBridgeEntry {
                index: index as u32,
                segment: bridge.segment,
                start_bus: bridge.start_bus,
                end_bus: bridge.end_bus,
                ecam_range: bridge.ecam,
                low_mmio: bridge.low_mmio,
                high_mmio: bridge.high_mmio,
                cxl: false,
                vnode: Some(0),
                preserve_boot_config: false,
            });
            // `host_dt::read` checks that this does not underflow.
            let bus_zero = bridge.ecam.start() - u64::from(bridge.start_bus) * BUS_SIZE;
            segments.push(McfgSegmentBusRange::new(
                bus_zero,
                bridge.segment,
                bridge.start_bus,
                bridge.end_bus,
            ));
        }
        builder.append(&Table::new_dyn(
            acpi_spec::mcfg::MCFG_REVISION,
            None,
            &McfgHeader::new(),
            &[segments.as_bytes()],
        ));
        builder.append_raw(&ssdt.to_bytes());
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_dt::tests::C_BIT;
    use crate::host_dt::tests::RAM_END;
    use crate::host_dt::tests::bridge;
    use crate::host_dt::tests::cells;
    use crate::host_dt::tests::device_tree;
    use crate::host_dt::tests::replace;
    use acpi::minimal::Cpu;
    use loader_defs::linux::SNP_BOOT_SHIM_ACPI_SIZE;
    use loader_defs::linux::SNP_BOOT_SHIM_MAX_CPUS;
    use loader_defs::linux::SNP_BOOT_SHIM_MAX_PCIE_BRIDGES;
    use test_with_tracing::test;
    use zerocopy::FromBytes;

    const OUTPUT: u64 = 0x20_0000;
    const HEADER_SIZE: usize = size_of::<acpi_spec::Header>();

    fn le64(bytes: &[u8]) -> u64 {
        u64::from_le_bytes(bytes[..8].try_into().unwrap())
    }

    fn table(tables: &[u8], address: u64) -> &[u8] {
        let bytes = &tables[(address - OUTPUT) as usize..];
        let (header, _) = acpi_spec::Header::read_from_prefix(bytes).unwrap();
        let table = &bytes[..header.length.get() as usize];
        assert_eq!(table.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)), 0);
        table
    }

    /// Returns the signatures and addresses of the XSDT entries.
    fn xsdt_entries<'a>(rsdp: &[u8], tables: &'a [u8]) -> Vec<(&'a [u8], u64)> {
        let rsdp = acpi_spec::Rsdp::read_from_bytes(rsdp).unwrap();
        let xsdt = table(tables, rsdp.xsdt);
        assert_eq!(&xsdt[..4], b"XSDT");
        xsdt[HEADER_SIZE..]
            .chunks_exact(8)
            .map(|entry| (&table(tables, le64(entry))[..4], le64(entry)))
            .collect()
    }

    fn build_from(dt: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let devices = crate::host_dt::read(dt, RAM_END, C_BIT).unwrap();
        build(&devices, OUTPUT)
    }

    fn has(bytes: &[u8], expected: &[u8]) -> bool {
        bytes
            .windows(expected.len())
            .any(|window| window == expected)
    }

    #[test]
    fn no_bridges_build_base_tables_only() {
        let (rsdp, tables) = build_from(&device_tree([false; 4], &[]));
        let entries = xsdt_entries(&rsdp, &tables);
        let signatures = entries.iter().map(|e| e.0).collect::<Vec<_>>();
        assert_eq!(signatures, [b"FACP", b"APIC", b"SRAT"]);
        let fadt = table(&tables, entries[0].1);
        assert_eq!(&table(&tables, le64(&fadt[140..]))[..4], b"DSDT");
    }

    #[test]
    fn bridges_build_mcfg_and_ssdt() {
        for count in [1, SNP_BOOT_SHIM_MAX_PCIE_BRIDGES as u32] {
            let bridges = (0..count).map(bridge).collect::<Vec<_>>();
            let (rsdp, tables) = build_from(&device_tree([false; 4], &bridges));
            let entries = xsdt_entries(&rsdp, &tables);
            assert_eq!(entries[3].0, b"MCFG");
            assert_eq!(entries[4].0, b"SSDT");
            let mut segment_count = 0;
            acpi_spec::mcfg::parse_mcfg(table(&tables, entries[3].1), |segment| {
                assert_eq!(segment.segment.get(), segment_count as u16);
                assert_eq!(
                    segment.ecam_base.get(),
                    0x4000_0000 + segment_count * 0x0100_0000
                );
                assert_eq!((segment.start_bus, segment.end_bus), (0, 15));
                segment_count += 1;
            })
            .unwrap();
            assert_eq!(segment_count, u64::from(count));
            let ssdt = table(&tables, entries[4].1);
            for index in 0..count as u64 {
                assert!(has(ssdt, &[b'P', b'C', b'I', b'0' + index as u8]));
                for base in [
                    0x4000_0000 + index * 0x0100_0000,
                    0x8000_0000 + index * 0x0100_0000,
                    (1 << 32) + index * 0x0100_0000,
                ] {
                    let resource = acpi::ssdt::QwordMemory::new(base, 0x0100_0000);
                    let mut bytes = Vec::new();
                    acpi::ssdt::ResourceObject::append_to_vec(&resource, &mut bytes);
                    assert!(has(ssdt, &bytes));
                }
            }
        }
    }

    #[test]
    fn mcfg_uses_the_bus_zero_ecam_base() {
        let mut properties = bridge(0);
        replace(&mut properties, "bus-range", cells(&[32, 47]));
        replace(&mut properties, "linux,pci-domain", cells(&[65535]));
        let (rsdp, tables) = build_from(&device_tree([false; 4], &[properties]));
        let entries = xsdt_entries(&rsdp, &tables);
        acpi_spec::mcfg::parse_mcfg(table(&tables, entries[3].1), |segment| {
            assert_eq!(segment.ecam_base.get(), 0x3e00_0000);
            assert_eq!((segment.start_bus, segment.end_bus), (32, 47));
            assert_eq!(segment.segment.get(), u16::MAX);
        })
        .unwrap();
        assert!(has(table(&tables, entries[4].1), b"\x08_BBN\x0a\x20"));
    }

    #[test]
    fn largest_configuration_fits_the_output() {
        let bridges = (0..SNP_BOOT_SHIM_MAX_PCIE_BRIDGES as u32)
            .map(bridge)
            .collect::<Vec<_>>();
        let dt = device_tree([true; 4], &bridges);
        let mut devices = crate::host_dt::read(&dt, RAM_END, C_BIT).unwrap();
        // Use x2APIC IDs and many RAM ranges, which make the largest entries.
        devices.cpus.clear();
        for id in 0..SNP_BOOT_SHIM_MAX_CPUS as u32 {
            devices.cpus.push(Cpu {
                id: id + 0x100,
                vnode: id % 4,
            });
        }
        while !devices.ram.is_full() {
            devices.ram.push(devices.ram[0]);
        }
        let (rsdp, tables) = build(&devices, OUTPUT);
        assert!(tables.len() <= SNP_BOOT_SHIM_ACPI_SIZE as usize);
        assert_eq!(xsdt_entries(&rsdp, &tables).len(), 5);
    }
}
