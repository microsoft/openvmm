// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Base ACPI tables for the fixed x86 enlightened SNP Linux chipset.
//!
//! [`Topology::new`] validates the small supported hardware contract without
//! allocating. The caller must first validate DT syntax, memory types, the
//! measured RAM interval, and unsupported devices. This is not a DT parser.
//! CPU order need not match APIC order; output always uses APIC ID + 1 for
//! the ACPI UID. Only UARTs supplied by the caller are emitted.
//!
//! [`Topology::build`] returns DSDT, MADT and SRAT bytes, with no guest pointers.
//! After assigning permanent storage to DSDT, call [`BaseTables::fadt`].
//! Append these tables and any PCIe tables built with the existing
//! [`crate::ssdt`] and `acpi_spec::mcfg` helpers to [`crate::builder::Builder`].
//! That builder supplies XSDT/RSDP. The caller must bound the complete output,
//! validate every destination and pointer, and publish RSDP only after copying
//! the complete set. These helpers neither accept RAM nor publish tables.
//!
//! The fixed devices match the hosted x86 table builder: APIC and RTC DSDT
//! resources, PIC compatibility and PIT IRQ0-to-GSI2 routing in MADT, BSP
//! LINT1 NMI, and the Hyper-V PM block in FADT. No VMBus, watchdog, PSP,
//! IOMMU, SLIT or PPTT is inferred.
//!
//! Hosted integration tests belong in `vmm_core`/`vm_manifest_builder`, not in
//! this crate (which they depend on). Before bootshim integration, compare
//! MADT/SRAT/FADT with `AcpiTablesBuilder` for 1 and 255 contiguous CPUs on node
//! zero; compare DSDT with the same APIC/RTC/CPU/sleep objects, allowing only
//! the UART shared-IRQ bit to differ. Check the actual
//! `EnlightenedLinuxDirect` manifest enables IOAPIC/PIC/PIT/RTC/PM with these
//! registers and rejects unsupported optional devices. Guest tests must
//! exercise COM1/COM3 and COM2/COM4 concurrently; AML byte tests alone do not
//! prove guest-driver interrupt sharing. The complete bootshim also needs a
//! combined 255-CPU/four-UART/eight-bridge output and allocation-budget test.

use crate::builder::OemInfo;
use crate::builder::Table;
use crate::dsdt;
use acpi_spec::fadt;
use acpi_spec::hyperv;
use acpi_spec::madt;
use acpi_spec::srat;
use alloc::vec;
use alloc::vec::Vec;
use thiserror::Error;
use x86defs::apic::APIC_BASE_ADDRESS;
use zerocopy::IntoBytes;

/// The fixed contract uses legacy APIC IDs 0 through 254.
pub const MAX_CPUS: usize = 255;
/// At most the four fixed COM ports can be present.
pub const MAX_UARTS: usize = 4;
/// Upper bound for DSDT + FADT + MADT + SRAT, including 8-byte table padding.
///
/// This does not include PCIe tables, XSDT/RSDP or temporary heap allocations.
pub const MAX_BASE_TABLE_BYTES: usize = 32 * 1024;

/// One CPU decoded from DT.
#[derive(Clone, Copy, Debug)]
pub struct Cpu {
    pub apic_id: u32,
    pub numa_node: u32,
    pub enabled: bool,
}

/// The single normalized RAM interval, already compared with measured RAM.
#[derive(Clone, Copy, Debug)]
pub struct Ram {
    pub start: u64,
    pub length: u64,
    pub numa_node: u32,
}

/// A present 16550 UART decoded from DT. `uid` is the COM number (1 to 4).
#[derive(Clone, Copy, Debug)]
pub struct Uart {
    pub uid: u8,
    pub io_base: u16,
    pub length: u64,
    pub irq: u32,
}

/// An unsupported or inconsistent SNP ACPI input.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("CPU count must match the measured count in 1 through 255")]
    CpuCount,
    #[error("BSP APIC ID must be zero")]
    Bsp,
    #[error(
        "CPUs must be enabled, unique, and have contiguous APIC IDs starting at zero on node zero"
    )]
    Cpu,
    #[error("RAM must be a nonempty, page-aligned interval on node zero without address overflow")]
    Ram,
    #[error("UARTs must be unique fixed COM1 through COM4 resources")]
    Uart,
    #[error("DSDT destination must be nonzero and its table must not overflow the address space")]
    DsdtAddress,
}

/// A validated topology. Construction and validation do not allocate.
pub struct Topology {
    cpu_count: u8,
    ram: Ram,
    uarts: [Option<Uart>; MAX_UARTS],
}

impl Topology {
    /// Checks the measured CPU count, BSP, CPU set, normalized RAM and UARTs.
    ///
    /// A missing UART list is not synthesized: an empty slice means no UARTs.
    /// Duplicate I/O resources are rejected; shared IRQ3/IRQ4 are valid.
    pub fn new(
        expected_cpu_count: u32,
        bsp_apic_id: u32,
        cpus: &[Cpu],
        ram: Ram,
        uarts: &[Uart],
    ) -> Result<Self, Error> {
        if !(1..=MAX_CPUS as u32).contains(&expected_cpu_count)
            || cpus.len() != expected_cpu_count as usize
        {
            return Err(Error::CpuCount);
        }
        if bsp_apic_id != 0 {
            return Err(Error::Bsp);
        }
        let mut seen = [false; MAX_CPUS];
        for cpu in cpus {
            if cpu.apic_id >= expected_cpu_count || cpu.numa_node != 0 || !cpu.enabled {
                return Err(Error::Cpu);
            }
            if core::mem::replace(&mut seen[cpu.apic_id as usize], true) {
                return Err(Error::Cpu);
            }
        }
        if ram.numa_node != 0
            || ram.length == 0
            || !ram.start.is_multiple_of(4096)
            || !ram.length.is_multiple_of(4096)
            || ram.start.checked_add(ram.length).is_none()
        {
            return Err(Error::Ram);
        }
        if uarts.len() > MAX_UARTS {
            return Err(Error::Uart);
        }
        let mut present = [None; MAX_UARTS];
        for uart in uarts {
            let index = match uart.uid {
                1..=4 => usize::from(uart.uid - 1),
                _ => return Err(Error::Uart),
            };
            if uart.io_base != x86defs::serial::COM_BASES[index]
                || uart.irq != u32::from(x86defs::serial::COM_IRQS[index])
                || uart.length != u64::from(x86defs::serial::COM_REGISTER_COUNT)
                || present[index].is_some()
            {
                return Err(Error::Uart);
            }
            present[index] = Some(*uart);
        }
        Ok(Self {
            cpu_count: expected_cpu_count as u8,
            ram,
            uarts: present,
        })
    }

    /// Builds the pointer-free base tables from validated, bounded inputs.
    ///
    /// The caller must provide an allocator with sufficient workspace. Input
    /// limits bound AML names and table sizes before entering the general AML
    /// helpers. The returned base tables fit [`MAX_BASE_TABLE_BYTES`].
    pub fn build(&self, oem: &OemInfo) -> BaseTables {
        let mut dsdt = dsdt::Dsdt::new();
        dsdt.add_object(&dsdt::NamedObject::new(
            b"\\_S0",
            &dsdt::Package(vec![0, 0]),
        ));
        dsdt.add_object(&dsdt::NamedObject::new(
            b"\\_S5",
            &dsdt::Package(vec![0, 0]),
        ));
        dsdt.add_apic();
        for uart in self.uarts.iter().flatten() {
            let name = [
                b'\\',
                b'_',
                b'S',
                b'B',
                b'.',
                b'U',
                b'A',
                b'R',
                b'0' + uart.uid,
            ];
            let ddn = [b'C', b'O', b'M', b'0' + uart.uid];
            dsdt.add_uart_shared(&name, &ddn, uart.uid.into(), uart.io_base, uart.irq);
        }
        dsdt.add_rtc();
        for uid in 1..=self.cpu_count {
            let name = [
                b'P',
                b'0' + uid / 100,
                b'0' + (uid / 10) % 10,
                b'0' + uid % 10,
            ];
            let mut cpu = dsdt::Device::new(&name);
            cpu.add_object(&dsdt::NamedString::new(b"_HID", b"ACPI0007"));
            cpu.add_object(&dsdt::NamedInteger::new(b"_UID", uid.into()));
            let mut sta = dsdt::Method::new(b"_STA");
            sta.add_operation(&dsdt::ReturnOp {
                result: dsdt::encode_integer(0xf),
            });
            cpu.add_object(&sta);
            dsdt.add_object(&cpu);
        }

        let mut madt_entries = Vec::with_capacity(
            size_of::<madt::MadtIoApic>()
                + 2 * size_of::<madt::MadtInterruptSourceOverride>()
                + size_of::<madt::MadtLocalNmiSource>()
                + usize::from(self.cpu_count) * size_of::<madt::MadtApic>(),
        );
        madt_entries.extend_from_slice(
            madt::MadtIoApic {
                io_apic_id: 0,
                io_apic_address: hyperv::IOAPIC_BASE_ADDRESS,
                ..madt::MadtIoApic::new()
            }
            .as_bytes(),
        );
        madt_entries.extend_from_slice(
            madt::MadtInterruptSourceOverride::new(
                hyperv::DEFAULT_ACPI_IRQ as u8,
                hyperv::DEFAULT_ACPI_IRQ,
                Some(madt::InterruptPolarity::ActiveHigh),
                Some(madt::InterruptTriggerMode::Level),
            )
            .as_bytes(),
        );
        madt_entries
            .extend_from_slice(madt::MadtInterruptSourceOverride::new(0, 2, None, None).as_bytes());
        madt_entries.extend_from_slice(madt::MadtLocalNmiSource::new().as_bytes());
        let mut srat_entries = Vec::with_capacity(
            usize::from(self.cpu_count) * size_of::<srat::SratApic>()
                + size_of::<srat::SratMemory>(),
        );
        for apic_id in 0..self.cpu_count {
            madt_entries.extend_from_slice(
                madt::MadtApic {
                    apic_id,
                    acpi_processor_uid: apic_id + 1,
                    flags: madt::MADT_APIC_ENABLED,
                    ..madt::MadtApic::new()
                }
                .as_bytes(),
            );
            srat_entries.extend_from_slice(srat::SratApic::new(apic_id, 0).as_bytes());
        }
        srat_entries.extend_from_slice(
            srat::SratMemory::new(self.ram.start, self.ram.length, 0).as_bytes(),
        );
        BaseTables {
            dsdt: dsdt.to_bytes(),
            madt: Table::new_dyn(
                5,
                None,
                &madt::Madt {
                    apic_addr: APIC_BASE_ADDRESS,
                    flags: madt::MADT_PCAT_COMPAT,
                },
                &[&madt_entries],
            )
            .to_vec(oem),
            srat: Table::new_dyn(
                srat::SRAT_REVISION,
                None,
                &srat::SratHeader::new(),
                &[&srat_entries],
            )
            .to_vec(oem),
        }
    }
}

/// Checksummed, unpadded base tables. DSDT is not an XSDT entry.
pub struct BaseTables {
    pub dsdt: Vec<u8>,
    pub madt: Vec<u8>,
    pub srat: Vec<u8>,
}

impl BaseTables {
    /// Builds a checksummed FADT referring to DSDT's permanent physical address.
    ///
    /// The legacy 32-bit DSDT pointer stays zero, as in the hosted builder.
    /// Only the extended pointer is used, including for DSDT above 4 GiB.
    /// This checks address overflow, not ownership or overlap of guest storage.
    pub fn fadt(&self, dsdt_address: u64, oem: &OemInfo) -> Result<Vec<u8>, Error> {
        if dsdt_address == 0 || dsdt_address.checked_add(self.dsdt.len() as u64).is_none() {
            return Err(Error::DsdtAddress);
        }
        Ok(Table::new(6, None, &fixed_fadt(dsdt_address)).to_vec(oem))
    }
}

fn pm_register(offset: u16, bits: u8, access_size: fadt::AddressWidth) -> fadt::GenericAddress {
    fadt::GenericAddress {
        addr_space_id: fadt::AddressSpaceId::SystemIo,
        register_bit_width: bits,
        register_bit_offset: 0,
        access_size,
        address: u64::from(hyperv::DEFAULT_PM_PIO_BASE + offset),
    }
}

fn fixed_fadt(dsdt_address: u64) -> fadt::Fadt {
    use fadt::AddressWidth;
    fadt::Fadt {
        flags: fadt::FADT_WBINVD
            | fadt::FADT_PROC_C1
            | fadt::FADT_PWR_BUTTON
            | fadt::FADT_SLP_BUTTON
            | fadt::FADT_RTC_S4
            | fadt::FADT_TMR_VAL_EXT
            | fadt::FADT_RESET_REG_SUP
            | fadt::FADT_USE_PLATFORM_CLOCK,
        x_dsdt: dsdt_address,
        sci_int: hyperv::DEFAULT_ACPI_IRQ as u16,
        p_lvl2_lat: 101,
        p_lvl3_lat: 1001,
        pm1_evt_len: 4,
        x_pm1a_evt_blk: pm_register(hyperv::PM_STATUS, 32, AddressWidth::Word),
        pm1_cnt_len: 2,
        x_pm1a_cnt_blk: pm_register(hyperv::PM_CONTROL, 16, AddressWidth::Word),
        gpe0_blk_len: 4,
        x_gpe0_blk: pm_register(hyperv::PM_GPE0_STATUS, 32, AddressWidth::Word),
        reset_reg: pm_register(hyperv::PM_RESET, 8, AddressWidth::Byte),
        reset_value: hyperv::RESET_VALUE,
        pm_tmr_len: 4,
        x_pm_tmr_blk: pm_register(hyperv::PM_TIMER, 32, AddressWidth::Dword),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use test_with_tracing::test;
    use zerocopy::FromBytes;

    const OEM: OemInfo = OemInfo {
        oem_id: *b"HVLITE",
        oem_tableid: *b"HVLITETB",
        oem_revision: 0,
        creator_id: *b"MSHV",
        creator_revision: 0,
    };
    const RAM: Ram = Ram {
        start: 0,
        length: 0x8000_0000,
        numa_node: 0,
    };
    const UARTS: [Uart; 4] = [
        Uart {
            uid: 1,
            io_base: 0x3f8,
            length: 8,
            irq: 4,
        },
        Uart {
            uid: 2,
            io_base: 0x2f8,
            length: 8,
            irq: 3,
        },
        Uart {
            uid: 3,
            io_base: 0x3e8,
            length: 8,
            irq: 4,
        },
        Uart {
            uid: 4,
            io_base: 0x2e8,
            length: 8,
            irq: 3,
        },
    ];

    fn cpus(count: u32) -> Vec<Cpu> {
        (0..count)
            .map(|apic_id| Cpu {
                apic_id,
                numa_node: 0,
                enabled: true,
            })
            .collect()
    }

    fn check_table(bytes: &[u8], signature: &[u8; 4], revision: u8) {
        let (header, _) = acpi_spec::Header::read_from_prefix(bytes).unwrap();
        assert_eq!(&header.signature, signature);
        assert_eq!(header.length.get() as usize, bytes.len());
        assert_eq!(header.revision, revision);
        assert_eq!(bytes.iter().fold(0u8, |sum, b| sum.wrapping_add(*b)), 0);
    }

    fn contains(bytes: &[u8], needle: &[u8]) -> bool {
        bytes.windows(needle.len()).any(|window| window == needle)
    }

    fn u16_at(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    #[test]
    fn rejects_invalid_cpu_contract() {
        for (expected, count) in [(0, 0), (256, 256), (1, 2), (2, 1)] {
            assert_eq!(
                Topology::new(expected, 0, &cpus(count), RAM, &[]).err(),
                Some(Error::CpuCount)
            );
        }
        assert_eq!(
            Topology::new(1, 1, &cpus(1), RAM, &[]).err(),
            Some(Error::Bsp)
        );
        for bad in [
            Cpu {
                apic_id: 2,
                numa_node: 0,
                enabled: true,
            },
            Cpu {
                apic_id: u32::MAX,
                numa_node: 0,
                enabled: true,
            },
            Cpu {
                apic_id: 1,
                numa_node: 1,
                enabled: true,
            },
            Cpu {
                apic_id: 1,
                numa_node: 0,
                enabled: false,
            },
            Cpu {
                apic_id: 0,
                numa_node: 0,
                enabled: true,
            },
        ] {
            let mut cpus = cpus(2);
            cpus[1] = bad;
            assert_eq!(Topology::new(2, 0, &cpus, RAM, &[]).err(), Some(Error::Cpu));
        }
    }

    #[test]
    fn rejects_invalid_ram_and_uart_resources() {
        for ram in [
            Ram { length: 0, ..RAM },
            Ram { start: 1, ..RAM },
            Ram {
                length: 4095,
                ..RAM
            },
            Ram {
                numa_node: 1,
                ..RAM
            },
            Ram {
                start: u64::MAX - 4095,
                length: 4096,
                ..RAM
            },
        ] {
            assert_eq!(
                Topology::new(1, 0, &cpus(1), ram, &[]).err(),
                Some(Error::Ram)
            );
        }
        for uart in [
            Uart { uid: 0, ..UARTS[0] },
            Uart { uid: 5, ..UARTS[0] },
            Uart {
                io_base: 0x3f9,
                ..UARTS[0]
            },
            Uart {
                io_base: 0x2f8,
                ..UARTS[0]
            },
            Uart {
                length: 0,
                ..UARTS[0]
            },
            Uart {
                length: 9,
                ..UARTS[0]
            },
            Uart { irq: 3, ..UARTS[0] },
        ] {
            assert_eq!(
                Topology::new(1, 0, &cpus(1), RAM, &[uart]).err(),
                Some(Error::Uart)
            );
        }
        for uarts in [&[UARTS[0]; 2][..], &[UARTS[0]; 5][..]] {
            assert_eq!(
                Topology::new(1, 0, &cpus(1), RAM, uarts).err(),
                Some(Error::Uart)
            );
        }
    }

    #[test]
    fn canonical_cpu_uids_affinities_and_maximum_size() {
        for count in [1, 2, 255] {
            let mut cpus = cpus(count);
            let tables = Topology::new(count, 0, &cpus, RAM, &UARTS)
                .unwrap()
                .build(&OEM);
            cpus.reverse();
            let mut uarts = UARTS;
            uarts.reverse();
            let reversed = Topology::new(count, 0, &cpus, RAM, &uarts)
                .unwrap()
                .build(&OEM);
            assert_eq!(tables.dsdt, reversed.dsdt);
            assert_eq!(tables.madt, reversed.madt);
            assert_eq!(tables.srat, reversed.srat);
            check_table(&tables.dsdt, b"DSDT", 2);
            check_table(&tables.madt, b"APIC", 5);
            check_table(&tables.srat, b"SRAT", 3);
            assert_eq!(u32_at(&tables.madt, 36), 0xfee0_0000);
            assert_eq!(u32_at(&tables.madt, 40), 1);
            // IOAPIC, SCI active-high/level, PIT override, then BSP LINT1 NMI.
            assert_eq!(
                &tables.madt[44..82],
                &[
                    1, 12, 0, 0, 0, 0, 0xc0, 0xfe, 0, 0, 0, 0, 2, 10, 0, 9, 9, 0, 0, 0, 13, 0, 2,
                    10, 0, 0, 2, 0, 0, 0, 0, 0, 4, 6, 1, 0, 0, 1,
                ]
            );
            assert_eq!(tables.madt.len(), 82 + count as usize * 8);
            assert_eq!(tables.srat.len(), 48 + count as usize * 16 + 40);
            for apic_id in 0..count as u8 {
                let uid = apic_id + 1;
                let offset = 82 + apic_id as usize * 8;
                let apic =
                    madt::MadtApic::read_from_bytes(&tables.madt[offset..offset + 8]).unwrap();
                assert_eq!(apic.apic_id, apic_id);
                assert_eq!(apic.acpi_processor_uid, uid);
                assert_eq!({ apic.flags }, 1);
                let offset = 48 + apic_id as usize * 16;
                assert_eq!(
                    &tables.srat[offset..offset + 16],
                    &[0, 16, 0, apic_id, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
                );
                let mut aml = format!("P{uid:03}").into_bytes();
                aml.extend_from_slice(b"\x08_HID\x0dACPI0007\0\x08_UID");
                if uid == 1 {
                    aml.push(1);
                } else {
                    aml.extend_from_slice(&[0xa, uid]);
                }
                assert!(contains(&tables.dsdt, &aml));
            }
            let offset = 48 + count as usize * 16;
            let memory = srat::SratMemory::read_from_bytes(&tables.srat[offset..]).unwrap();
            assert_eq!(memory.proximity_domain.get(), 0);
            assert_eq!(memory.low_address.get(), 0);
            assert_eq!(memory.high_address.get(), 0);
            assert_eq!(memory.low_length.get(), RAM.length as u32);
            assert_eq!(memory.high_length.get(), 0);
            assert_eq!(memory.flags.get(), 1);
            let fadt = tables.fadt(0x1000_0000, &OEM).unwrap();
            let size: usize = [&tables.dsdt, &tables.madt, &tables.srat, &fadt]
                .iter()
                .map(|table| table.len().next_multiple_of(8))
                .sum();
            assert!(size <= MAX_BASE_TABLE_BYTES, "{size}");
        }
    }

    #[test]
    fn uart_presence_and_shared_irq_leave_legacy_bytes_unchanged() {
        let empty = Topology::new(1, 0, &cpus(1), RAM, &[]).unwrap().build(&OEM);
        assert!(!contains(&empty.dsdt, b"UAR"));
        for uart in UARTS {
            let only = Topology::new(1, 0, &cpus(1), RAM, &[uart])
                .unwrap()
                .build(&OEM);
            for uid in 1..=4 {
                assert_eq!(
                    contains(&only.dsdt, format!("UAR{uid}").as_bytes()),
                    uid == uart.uid
                );
            }
        }
        let all = Topology::new(1, 0, &cpus(1), RAM, &UARTS)
            .unwrap()
            .build(&OEM);
        for irq in [3, 4] {
            let resource = [0x89, 6, 0, 0xb, 1, irq, 0, 0, 0];
            assert_eq!(
                all.dsdt
                    .windows(resource.len())
                    .filter(|w| *w == resource)
                    .count(),
                2
            );
        }
        let mut legacy = dsdt::Dsdt::new();
        legacy.add_uart(b"\\_SB.UAR1", b"COM1", 1, 0x3f8, 4);
        let legacy = legacy.to_bytes();
        assert!(contains(&legacy, &[0x89, 6, 0, 3, 1, 4, 0, 0, 0]));
        let mut shared = dsdt::Dsdt::new();
        shared.add_uart_shared(b"\\_SB.UAR1", b"COM1", 1, 0x3f8, 4);
        let shared = shared.to_bytes();
        let differences: Vec<_> = legacy
            .iter()
            .zip(&shared)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .collect();
        assert_eq!(differences.len(), 2);
        assert_eq!(differences[0].0, 9); // checksum
        assert_eq!((*differences[1].1.0, *differences[1].1.1), (3, 11));
        for (start, length) in [(0xfee0_0000u32, 4096u32), (0xfec0_0000, 4096)] {
            let mut memory = vec![0x86, 9, 0, 1];
            memory.extend_from_slice(&start.to_le_bytes());
            memory.extend_from_slice(&length.to_le_bytes());
            assert!(contains(&all.dsdt, &memory));
        }
        assert!(contains(&all.dsdt, &[0x47, 1, 0x70, 0, 0x70, 0, 0, 2]));
        assert!(contains(&all.dsdt, &[0x89, 6, 0, 3, 1, 8, 0, 0, 0]));
        for sleep in [b"\\_S0_", b"\\_S5_"] {
            let mut aml = vec![8];
            aml.extend_from_slice(sleep);
            aml.extend_from_slice(&[0x12, 4, 2, 0, 0]);
            assert!(contains(&all.dsdt, &aml));
        }
    }

    #[test]
    fn fadt_fixed_registers_and_full_width_dsdt_pointer() {
        let tables = Topology::new(1, 0, &cpus(1), RAM, &[]).unwrap().build(&OEM);
        for address in [0x2000, 0x1_0000_2000] {
            let fadt = tables.fadt(address, &OEM).unwrap();
            check_table(&fadt, b"FACP", 6);
            assert_eq!(fadt.len(), 276);
            assert_eq!(u32_at(&fadt, 40), 0);
            assert_eq!(u64_at(&fadt, 140), address);
            assert_eq!(u16_at(&fadt, 46), 9);
            assert_eq!(&fadt[88..96], &[4, 2, 0, 4, 4, 0, 0, 0]);
            assert_eq!(u16_at(&fadt, 96), 101);
            assert_eq!(u16_at(&fadt, 98), 1001);
            assert_eq!(u32_at(&fadt, 112), 0x85b5);
            for (offset, bits, width, port) in [
                (116, 8, 1, 0x433),
                (148, 32, 2, 0x400),
                (172, 16, 2, 0x404),
                (208, 32, 3, 0x408),
                (220, 32, 2, 0x40c),
            ] {
                assert_eq!(&fadt[offset..offset + 4], &[1, bits, 0, width]);
                assert_eq!(u64_at(&fadt, offset + 4), port);
            }
            assert_eq!(fadt[128], 1);
        }
        assert_eq!(tables.fadt(0, &OEM).err(), Some(Error::DsdtAddress));
        assert_eq!(tables.fadt(u64::MAX, &OEM).err(), Some(Error::DsdtAddress));
    }

    #[test]
    fn base_tables_compose_with_existing_root_builder() {
        let tables = Topology::new(2, 0, &cpus(2), RAM, &[]).unwrap().build(&OEM);
        let base = 0x1_0000_0000;
        let mut builder = crate::builder::Builder::new(base, OEM);
        let dsdt = builder.append_raw(&tables.dsdt);
        let fadt_bytes = tables.fadt(dsdt, &OEM).unwrap();
        let fadt = builder.append_raw(&fadt_bytes);
        let madt = builder.append_raw(&tables.madt);
        let srat = builder.append_raw(&tables.srat);
        let (rsdp_bytes, bytes) = builder.build();
        let rsdp = acpi_spec::Rsdp::read_from_bytes(&rsdp_bytes).unwrap();
        assert_eq!(
            rsdp_bytes[..20].iter().fold(0u8, |a, b| a.wrapping_add(*b)),
            0
        );
        assert_eq!(rsdp_bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
        let offset = (rsdp.xsdt - base) as usize;
        let length = u32_at(&bytes, offset + 4) as usize;
        let xsdt = &bytes[offset..offset + length];
        check_table(xsdt, b"XSDT", 1);
        assert_eq!(&xsdt[36..], [fadt, madt, srat].as_bytes());
        assert_eq!(u64_at(&fadt_bytes, 140), dsdt);
        for (address, expected) in [
            (dsdt, &tables.dsdt),
            (fadt, &fadt_bytes),
            (madt, &tables.madt),
            (srat, &tables.srat),
        ] {
            assert_eq!(address % 8, 0);
            let offset = (address - base) as usize;
            assert_eq!(&bytes[offset..offset + expected.len()], expected);
        }
    }
}
