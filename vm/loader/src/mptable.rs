// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel MP 1.4 table construction for x86 guests.

mod spec;

use spec::BUS_TYPE_ISA;
use spec::BusEntry;
use spec::CONFIG_TABLE_SIGNATURE;
use spec::CPU_FLAG_BOOTSTRAP;
use spec::CPU_FLAG_ENABLED;
use spec::ConfigTableHeader;
use spec::ENTRY_TYPE_BUS;
use spec::ENTRY_TYPE_IO_APIC;
use spec::ENTRY_TYPE_IO_INTERRUPT;
use spec::ENTRY_TYPE_PROCESSOR;
use spec::FLOATING_POINTER_SIGNATURE;
use spec::FloatingPointer;
use spec::INTERRUPT_FLAG_ACTIVE_HIGH;
use spec::INTERRUPT_FLAG_EDGE_TRIGGERED;
use spec::INTERRUPT_FLAG_LEVEL_TRIGGERED;
use spec::INTERRUPT_TYPE_INT;
use spec::IO_APIC_FLAG_ENABLED;
use spec::IoApicEntry;
use spec::IoInterruptEntry;
use spec::PARAGRAPH_SIZE;
use spec::ProcessorEntry;
use spec::SPEC_REV_1_4;
use thiserror::Error;
use x86defs::apic::APIC_BASE_ADDRESS;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Guest-physical address of the MP floating pointer.
pub const MP_FLOATING_POINTER_ADDR: usize = 0;
/// Guest-physical address of the MP configuration table.
pub const MP_CONFIG_TABLE_ADDR: usize = 0x400;

/// Configuration table OEM ID, space-padded.
const OEM_ID: [u8; 8] = *b"OPENVMM ";
/// Configuration table product ID, space-padded.
const PRODUCT_ID: [u8; 12] = *b"MICROVM     ";
/// Low byte of the local APIC version register: an integrated xAPIC
/// (`0x1X`) of version `0x14`, as every backend reports.
const LOCAL_APIC_VERSION: u8 = 0x14;
/// Version of the emulated Intel 82093AA I/O APIC, matching
/// `chipset::ioapic`.
const IO_APIC_VERSION: u8 = 0x11;
/// Base address of the I/O APIC, matching
/// `chipset::ioapic::IOAPIC_DEVICE_MMIO_REGION_BASE_ADDRESS`.
const IO_APIC_ADDRESS: u32 = 0xfec0_0000;
/// The I/O APIC ID register holds four bits.
const MAX_IO_APIC_ID: u8 = 0xf;
/// Bus ID of the ISA bus, the only bus described.
const ISA_BUS_ID: u8 = 0;
/// Number of ISA IRQs.
const ISA_IRQ_COUNT: u8 = 16;
/// ISA IRQ 2 cascades the secondary PIC and has no I/O APIC route.
const ISA_CASCADE_IRQ: u8 = 2;
/// ISA IRQ of the PIT.
const PIT_IRQ: u8 = 0;
/// I/O APIC input that the PIT's ISA IRQ is wired to.
const PIT_IO_APIC_INTIN: u8 = 2;
/// Number of routed ISA IRQs: every ISA IRQ except the cascade.
const ISA_ROUTED_IRQ_COUNT: usize = ISA_IRQ_COUNT as usize - 1;

/// The fixed entries that follow the processor entries: the ISA bus, the
/// I/O APIC, and the route of each ISA IRQ to the I/O APIC.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct IsaInterruptRouting {
    bus: BusEntry,
    io_apic: IoApicEntry,
    interrupts: [IoInterruptEntry; ISA_ROUTED_IRQ_COUNT],
}

/// Number of entries in [`IsaInterruptRouting`].
const ISA_INTERRUPT_ROUTING_ENTRY_COUNT: usize = 2 + ISA_ROUTED_IRQ_COUNT;

/// Guest-visible processor and interrupt data represented by MP tables.
#[derive(Debug, Clone, Copy)]
pub struct MpTableConfig<'a> {
    /// APIC IDs in virtual-processor order. The first entry is the BSP. The
    /// IDs must be 0, 1, 2, and so on, for 1 to 15 processors, because the
    /// I/O APIC takes the next ID, which must fit in four bits.
    pub apic_ids: &'a [u32],
    /// ISA IRQs, 0 through 15, described as active-high, level-triggered.
    pub level_triggered_irqs: &'a [u32],
    /// The number of inputs of the IOAPIC: at least the 16 that carry the ISA
    /// IRQs. The table routes each input past those as an active-high,
    /// edge-triggered interrupt of its own number, which the PIC does not see.
    pub ioapic_inputs: u8,
}

/// Fully constructed MP 1.4 data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpTables {
    /// The 16-byte MP floating pointer.
    pub floating_pointer: [u8; 16],
    /// The MP configuration table referenced by the floating pointer.
    pub configuration_table: Vec<u8>,
}

/// MP table construction error.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// At least one processor must be described.
    #[error("MP processor topology must contain at least one processor")]
    NoProcessors,
    /// APIC IDs must be contiguous and start at zero.
    #[error("MP processor {index} has APIC ID {apic_id}; expected {expected}")]
    InvalidApicId {
        /// Processor index.
        index: usize,
        /// Supplied APIC ID.
        apic_id: u32,
        /// Required APIC ID.
        expected: u32,
    },
    /// MP 1.4 processor entries contain only an 8-bit APIC ID.
    #[error("MP processor {index} APIC ID {apic_id} does not fit in an MP 1.4 entry")]
    ApicIdTooLarge {
        /// Processor index.
        index: usize,
        /// Supplied APIC ID.
        apic_id: u32,
    },
    /// The entry count or table length cannot be represented.
    #[error("MP table has too many processor or interrupt entries")]
    TooManyEntries,
    /// The I/O APIC ID register exposes only four ID bits.
    #[error("MP topology has {processor_count} processors, leaving no representable I/O APIC ID")]
    NoIoApicId {
        /// Number of processor APIC IDs already in use.
        processor_count: usize,
    },
    /// ISA IRQs are limited to 0 through 15.
    #[error("MP table IRQ {0} is outside the ISA range")]
    InvalidIrq(u32),
    /// The IOAPIC's first 16 inputs carry the ISA IRQs.
    #[error("MP table IOAPIC has {0} inputs; the ISA IRQs need 16")]
    TooFewIoApicInputs(u8),
}

/// Builds the MP floating pointer and configuration table.
pub fn build(config: &MpTableConfig<'_>) -> Result<MpTables, Error> {
    let configuration_table = build_config_table(config)?;
    let mut floating_pointer = FloatingPointer {
        signature: FLOATING_POINTER_SIGNATURE,
        config_table_address: (MP_CONFIG_TABLE_ADDR as u32).into(),
        length: (size_of::<FloatingPointer>() / PARAGRAPH_SIZE) as u8,
        spec_rev: SPEC_REV_1_4,
        // A configuration table is present, and the IMCR is absent, which
        // selects virtual wire mode.
        ..FromZeros::new_zeroed()
    };
    floating_pointer.checksum = checksum(&[floating_pointer.as_bytes()]);
    Ok(MpTables {
        floating_pointer: zerocopy::transmute!(floating_pointer),
        configuration_table,
    })
}

/// Builds an Intel MP 1.4 configuration table.
pub fn build_config_table(config: &MpTableConfig<'_>) -> Result<Vec<u8>, Error> {
    if config.apic_ids.is_empty() {
        return Err(Error::NoProcessors);
    }
    if let Some(&irq) = config
        .level_triggered_irqs
        .iter()
        .find(|irq| **irq >= ISA_IRQ_COUNT.into())
    {
        return Err(Error::InvalidIrq(irq));
    }
    if config.ioapic_inputs < ISA_IRQ_COUNT {
        return Err(Error::TooFewIoApicInputs(config.ioapic_inputs));
    }
    // One bus, one IOAPIC, and an interrupt source for every ISA IRQ but the
    // PIC cascade (IRQ 2) and for every IOAPIC input past the ISA IRQs.
    let entry_count = u16::try_from(
        config
            .apic_ids
            .len()
            .checked_add(ISA_INTERRUPT_ROUTING_ENTRY_COUNT)
            .and_then(|count| count.checked_add(usize::from(config.ioapic_inputs - ISA_IRQ_COUNT)))
            .ok_or(Error::TooManyEntries)?,
    )
    .map_err(|_| Error::TooManyEntries)?;

    let processors = config
        .apic_ids
        .iter()
        .enumerate()
        .map(|(index, &apic_id)| {
            let expected = u32::try_from(index).map_err(|_| Error::TooManyEntries)?;
            if apic_id != expected {
                return Err(Error::InvalidApicId {
                    index,
                    apic_id,
                    expected,
                });
            }
            let local_apic_id =
                u8::try_from(apic_id).map_err(|_| Error::ApicIdTooLarge { index, apic_id })?;
            let bootstrap = if index == 0 { CPU_FLAG_BOOTSTRAP } else { 0 };
            Ok(ProcessorEntry {
                entry_type: ENTRY_TYPE_PROCESSOR,
                local_apic_id,
                local_apic_version: LOCAL_APIC_VERSION,
                cpu_flags: CPU_FLAG_ENABLED | bootstrap,
                ..FromZeros::new_zeroed()
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let io_apic_id = u8::try_from(config.apic_ids.len())
        .ok()
        .filter(|id| *id <= MAX_IO_APIC_ID)
        .ok_or(Error::NoIoApicId {
            processor_count: config.apic_ids.len(),
        })?;
    let mut routing = IsaInterruptRouting {
        bus: BusEntry {
            entry_type: ENTRY_TYPE_BUS,
            bus_id: ISA_BUS_ID,
            bus_type: BUS_TYPE_ISA,
        },
        io_apic: IoApicEntry {
            entry_type: ENTRY_TYPE_IO_APIC,
            io_apic_id,
            io_apic_version: IO_APIC_VERSION,
            flags: IO_APIC_FLAG_ENABLED,
            address: IO_APIC_ADDRESS.into(),
        },
        interrupts: FromZeros::new_zeroed(),
    };
    let routed_irqs = (0..ISA_IRQ_COUNT).filter(|irq| *irq != ISA_CASCADE_IRQ);
    for (entry, irq) in routing.interrupts.iter_mut().zip(routed_irqs) {
        let flags = if config.level_triggered_irqs.contains(&irq.into()) {
            INTERRUPT_FLAG_ACTIVE_HIGH | INTERRUPT_FLAG_LEVEL_TRIGGERED
        } else {
            0
        };
        *entry = IoInterruptEntry {
            entry_type: ENTRY_TYPE_IO_INTERRUPT,
            interrupt_type: INTERRUPT_TYPE_INT,
            flags: flags.into(),
            source_bus_id: ISA_BUS_ID,
            source_bus_irq: irq,
            destination_io_apic_id: io_apic_id,
            destination_io_apic_intin: if irq == PIT_IRQ {
                PIT_IO_APIC_INTIN
            } else {
                irq
            },
        };
    }

    // An input past the ISA IRQs has no bus convention to conform to, so its
    // entry states its polarity and trigger. Linux takes the source IRQ of a
    // non-PCI bus, here the input's own number, as the interrupt's number.
    let inputs = (ISA_IRQ_COUNT..config.ioapic_inputs)
        .map(|input| IoInterruptEntry {
            entry_type: ENTRY_TYPE_IO_INTERRUPT,
            interrupt_type: INTERRUPT_TYPE_INT,
            flags: (INTERRUPT_FLAG_ACTIVE_HIGH | INTERRUPT_FLAG_EDGE_TRIGGERED).into(),
            source_bus_id: ISA_BUS_ID,
            source_bus_irq: input,
            destination_io_apic_id: io_apic_id,
            destination_io_apic_intin: input,
        })
        .collect::<Vec<_>>();

    let table_len = size_of::<ConfigTableHeader>()
        + processors.as_bytes().len()
        + size_of::<IsaInterruptRouting>()
        + inputs.as_bytes().len();
    let mut header = ConfigTableHeader {
        signature: CONFIG_TABLE_SIGNATURE,
        base_table_length: u16::try_from(table_len)
            .map_err(|_| Error::TooManyEntries)?
            .into(),
        spec_rev: SPEC_REV_1_4,
        oem_id: OEM_ID,
        product_id: PRODUCT_ID,
        entry_count: entry_count.into(),
        local_apic_address: APIC_BASE_ADDRESS.into(),
        ..FromZeros::new_zeroed()
    };
    let parts = [
        header.as_bytes(),
        processors.as_bytes(),
        routing.as_bytes(),
        inputs.as_bytes(),
    ];
    header.checksum = checksum(&parts);
    Ok([
        header.as_bytes(),
        processors.as_bytes(),
        routing.as_bytes(),
        inputs.as_bytes(),
    ]
    .concat())
}

/// Returns the byte that makes the sum of `parts` and itself zero.
fn checksum(parts: &[&[u8]]) -> u8 {
    0u8.wrapping_sub(
        parts
            .iter()
            .flat_map(|part| part.iter())
            .fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    /// The inputs of the microVM's IOAPIC.
    const IOAPIC_INPUTS: u8 = 24;

    /// Returns the interrupt-source entries of `table`, which describes
    /// `processor_count` processors, in table order.
    fn interrupt_sources(
        table: &[u8],
        processor_count: usize,
    ) -> Vec<[u8; size_of::<IoInterruptEntry>()]> {
        table[size_of::<ConfigTableHeader>() + size_of::<ProcessorEntry>() * processor_count..]
            .chunks_exact(size_of::<IoInterruptEntry>())
            .map(|entry| entry.try_into().unwrap())
            .filter(|entry: &[u8; size_of::<IoInterruptEntry>()]| {
                entry[0] == ENTRY_TYPE_IO_INTERRUPT
            })
            .collect()
    }

    #[test]
    fn builds_valid_checksums_and_processor_entries() {
        for processor_count in [1usize, 2, 4, 8, 15] {
            let apic_ids = (0..processor_count as u32).collect::<Vec<_>>();
            let tables = build(&MpTableConfig {
                apic_ids: &apic_ids,
                level_triggered_irqs: &[4, 5, 6, 7],
                ioapic_inputs: IOAPIC_INPUTS,
            })
            .unwrap();

            let floating_pointer =
                FloatingPointer::read_from_bytes(&tables.floating_pointer).unwrap();
            assert_eq!(floating_pointer.signature, FLOATING_POINTER_SIGNATURE);
            assert_eq!(
                floating_pointer.config_table_address.get(),
                MP_CONFIG_TABLE_ADDR as u32
            );
            assert_eq!(checksum(&[&tables.floating_pointer]), 0);

            let table = &tables.configuration_table;
            assert_eq!(checksum(&[table]), 0);
            let (header, rest) = ConfigTableHeader::ref_from_prefix(table).unwrap();
            assert_eq!(usize::from(header.base_table_length.get()), table.len());
            // The processors, the bus, the IOAPIC, and 23 interrupt sources:
            // 15 ISA IRQs and inputs 16 through 23.
            assert_eq!(
                usize::from(header.entry_count.get()),
                processor_count + ISA_INTERRUPT_ROUTING_ENTRY_COUNT + 8
            );
            let (processors, rest) =
                <[ProcessorEntry]>::ref_from_prefix_with_elems(rest, processor_count).unwrap();
            for (index, processor) in processors.iter().enumerate() {
                assert_eq!(processor.entry_type, ENTRY_TYPE_PROCESSOR);
                assert_eq!(processor.local_apic_id, index as u8);
                let bootstrap = if index == 0 { CPU_FLAG_BOOTSTRAP } else { 0 };
                assert_eq!(processor.cpu_flags, CPU_FLAG_ENABLED | bootstrap);
            }
            let (routing, rest) = IsaInterruptRouting::ref_from_prefix(rest).unwrap();
            assert_eq!(routing.io_apic.entry_type, ENTRY_TYPE_IO_APIC);
            assert_eq!(routing.io_apic.io_apic_id, processor_count as u8);
            let inputs = <[IoInterruptEntry]>::ref_from_bytes_with_elems(rest, 8).unwrap();
            for entry in routing.interrupts.iter().chain(inputs) {
                assert_eq!(entry.entry_type, ENTRY_TYPE_IO_INTERRUPT);
                assert_eq!(entry.destination_io_apic_id, processor_count as u8);
            }
        }
    }

    /// Pins the guest-visible bytes of two processors with ISA IRQ 5
    /// active-high and level-triggered.
    #[test]
    fn builds_expected_bytes() {
        let tables = build(&MpTableConfig {
            apic_ids: &[0, 1],
            level_triggered_irqs: &[5],
            ioapic_inputs: 16,
        })
        .unwrap();
        let hex = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        assert_eq!(
            hex(&tables.floating_pointer),
            "5f4d505f0004000001049c0000000000"
        );
        assert_eq!(
            hex(&tables.configuration_table),
            concat!(
                // Header: "PCMP", 220 bytes, revision 1.4, checksum, OEM ID,
                // and product ID.
                "50434d50dc00047b4f50454e564d4d204d4943524f564d2020202020",
                // No OEM table, 19 entries, the local APIC at 0xfee00000,
                // and no extended table.
                "00000000000013000000e0fe00000000",
                // Processors: the BSP (enabled, BP) and one AP (enabled).
                "0000140300000000000000000000000000000000",
                "0001140100000000000000000000000000000000",
                // ISA bus 0 and the I/O APIC (ID 2, version 0x11, enabled).
                "0100495341202020",
                "020211010000c0fe",
                // ISA IRQs 0-15 except 2; IRQ 0 goes to INTIN 2.
                "0300000000000202",
                "0300000000010201",
                "0300000000030203",
                "0300000000040204",
                "03000d0000050205",
                "0300000000060206",
                "0300000000070207",
                "0300000000080208",
                "0300000000090209",
                "03000000000a020a",
                "03000000000b020b",
                "03000000000c020c",
                "03000000000d020d",
                "03000000000e020e",
                "03000000000f020f",
            )
        );
    }

    #[test]
    fn routes_every_ioapic_input() {
        let table = build_config_table(&MpTableConfig {
            apic_ids: &[0, 1],
            level_triggered_irqs: &[5],
            ioapic_inputs: IOAPIC_INPUTS,
        })
        .unwrap();
        // Each entry: type, interrupt type, flags, source bus, source IRQ,
        // destination IOAPIC, and destination input. The IOAPIC takes ID 2,
        // after the processors. The PIT's IRQ 0 reaches input 2, and IRQ 2,
        // the PIC cascade, has no entry.
        let mut expected = vec![[3, 0, 0, 0, 0, 0, 2, 2]];
        for irq in 1..16 {
            if irq != 2 {
                let flags = if irq == 5 { 0x0d } else { 0 };
                expected.push([3, 0, flags, 0, 0, irq, 2, irq]);
            }
        }
        // The inputs past the ISA IRQs are active high and edge triggered.
        for input in 16..24 {
            expected.push([3, 0, 0x05, 0, 0, input, 2, input]);
        }
        assert_eq!(interrupt_sources(&table, 2), expected);
    }

    #[test]
    fn routes_the_isa_irqs_alone_without_further_inputs() {
        let table = build_config_table(&MpTableConfig {
            apic_ids: &[0],
            level_triggered_irqs: &[],
            ioapic_inputs: 16,
        })
        .unwrap();
        assert_eq!(
            table.len(),
            size_of::<ConfigTableHeader>()
                + size_of::<ProcessorEntry>()
                + size_of::<IsaInterruptRouting>()
        );
        let sources = interrupt_sources(&table, 1);
        assert_eq!(sources.len(), 15);
        assert!(
            sources
                .iter()
                .all(|source| source[5] < 16 && source[7] < 16)
        );
    }

    #[test]
    fn rejects_invalid_inputs() {
        assert_eq!(
            build(&MpTableConfig {
                apic_ids: &[],
                level_triggered_irqs: &[],
                ioapic_inputs: IOAPIC_INPUTS,
            }),
            Err(Error::NoProcessors)
        );
        assert!(matches!(
            build(&MpTableConfig {
                apic_ids: &[0, 2],
                level_triggered_irqs: &[],
                ioapic_inputs: IOAPIC_INPUTS,
            }),
            Err(Error::InvalidApicId { .. })
        ));
        assert_eq!(
            build(&MpTableConfig {
                apic_ids: &[0],
                level_triggered_irqs: &[16],
                ioapic_inputs: IOAPIC_INPUTS,
            }),
            Err(Error::InvalidIrq(16))
        );
        assert_eq!(
            build(&MpTableConfig {
                apic_ids: &[0],
                level_triggered_irqs: &[],
                ioapic_inputs: 15,
            }),
            Err(Error::TooFewIoApicInputs(15))
        );
        let apic_ids = (0..16).collect::<Vec<_>>();
        assert_eq!(
            build(&MpTableConfig {
                apic_ids: &apic_ids,
                level_triggered_irqs: &[],
                ioapic_inputs: IOAPIC_INPUTS,
            }),
            Err(Error::NoIoApicId {
                processor_count: 16
            })
        );
        let apic_ids = (0..=256).collect::<Vec<_>>();
        assert!(matches!(
            build(&MpTableConfig {
                apic_ids: &apic_ids,
                level_triggered_irqs: &[],
                ioapic_inputs: IOAPIC_INPUTS,
            }),
            Err(Error::ApicIdTooLarge {
                index: 256,
                apic_id: 256
            })
        ));
    }
}
