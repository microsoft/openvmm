// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel MP 1.4 table construction for x86 guests.

use thiserror::Error;

/// Guest-physical address of the MP floating pointer.
pub const MP_FLOATING_POINTER_ADDR: usize = 0;
/// Guest-physical address of the MP configuration table.
pub const MP_CONFIG_TABLE_ADDR: usize = 0x400;

const MP_IRQ_FLAGS_LEVEL_HIGH: u16 = 0x000d;
/// Active-high polarity (bits 1:0 = 1) and edge trigger (bits 3:2 = 1).
const MP_IRQ_FLAGS_EDGE_HIGH: u16 = 0x0005;
const MP_CONFIG_HEADER_SIZE: usize = 44;
const MP_PROCESSOR_SIZE: usize = 20;
/// The size of a bus, IOAPIC, or interrupt-source entry.
const MP_ENTRY_SIZE: usize = 8;
/// The ISA IRQs, which the first IOAPIC inputs carry.
const ISA_IRQ_COUNT: u8 = 16;

/// Guest-visible processor and interrupt data represented by MP tables.
#[derive(Debug, Clone, Copy)]
pub struct MpTableConfig<'a> {
    /// APIC IDs in virtual-processor order. The first entry is the BSP.
    pub apic_ids: &'a [u32],
    /// ISA IRQs described as active-high, level-triggered.
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
    /// MicroVM APIC IDs must be contiguous and start at zero.
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
    let mut floating_pointer = [0u8; 16];
    floating_pointer[..4].copy_from_slice(b"_MP_");
    floating_pointer[4..8].copy_from_slice(&(MP_CONFIG_TABLE_ADDR as u32).to_le_bytes());
    floating_pointer[8] = 1;
    floating_pointer[9] = 4;
    floating_pointer[10] = checksum(&floating_pointer);
    Ok(MpTables {
        floating_pointer,
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
        .find(|irq| **irq >= u32::from(ISA_IRQ_COUNT))
    {
        return Err(Error::InvalidIrq(irq));
    }
    if config.ioapic_inputs < ISA_IRQ_COUNT {
        return Err(Error::TooFewIoApicInputs(config.ioapic_inputs));
    }
    // One bus, one IOAPIC, and an interrupt source for every ISA IRQ but the
    // PIC cascade (IRQ 2) and for every IOAPIC input past the ISA IRQs.
    let non_processor_entry_count =
        2 + usize::from(ISA_IRQ_COUNT - 1) + usize::from(config.ioapic_inputs - ISA_IRQ_COUNT);
    let entry_count = u16::try_from(
        config
            .apic_ids
            .len()
            .checked_add(non_processor_entry_count)
            .ok_or(Error::TooManyEntries)?,
    )
    .map_err(|_| Error::TooManyEntries)?;

    let capacity = MP_CONFIG_HEADER_SIZE
        .checked_add(
            MP_PROCESSOR_SIZE
                .checked_mul(config.apic_ids.len())
                .ok_or(Error::TooManyEntries)?,
        )
        .and_then(|size| size.checked_add(MP_ENTRY_SIZE * non_processor_entry_count))
        .ok_or(Error::TooManyEntries)?;
    let mut table = Vec::with_capacity(capacity);
    table.extend_from_slice(b"PCMP");
    table.extend_from_slice(&0u16.to_le_bytes());
    table.push(4);
    table.push(0);
    table.extend_from_slice(b"OPENVMM ");
    table.extend_from_slice(b"MICROVM     ");
    table.extend_from_slice(&0u32.to_le_bytes());
    table.extend_from_slice(&0u16.to_le_bytes());
    table.extend_from_slice(&entry_count.to_le_bytes());
    table.extend_from_slice(&0xfee0_0000u32.to_le_bytes());
    table.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(table.len(), MP_CONFIG_HEADER_SIZE);

    for (index, &apic_id) in config.apic_ids.iter().enumerate() {
        let expected = u32::try_from(index).map_err(|_| Error::TooManyEntries)?;
        if apic_id != expected {
            return Err(Error::InvalidApicId {
                index,
                apic_id,
                expected,
            });
        }
        let apic_id =
            u8::try_from(apic_id).map_err(|_| Error::ApicIdTooLarge { index, apic_id })?;
        table.extend_from_slice(&[0, apic_id, 0x14, if index == 0 { 3 } else { 1 }]);
        table.extend_from_slice(&0u32.to_le_bytes());
        table.extend_from_slice(&0u32.to_le_bytes());
        table.extend_from_slice(&[0; 8]);
    }
    assert_eq!(
        table.len(),
        MP_CONFIG_HEADER_SIZE + MP_PROCESSOR_SIZE * config.apic_ids.len()
    );

    table.extend_from_slice(&[1, 0]);
    table.extend_from_slice(b"ISA   ");
    table.extend_from_slice(&[2, 0, 0x11, 1]);
    table.extend_from_slice(&0xfec0_0000u32.to_le_bytes());

    for irq in (0..ISA_IRQ_COUNT).filter(|irq| *irq != 2) {
        let pin = if irq == 0 { 2 } else { irq };
        let flags = if config.level_triggered_irqs.contains(&u32::from(irq)) {
            MP_IRQ_FLAGS_LEVEL_HIGH
        } else {
            0
        };
        table.extend_from_slice(&[3, 0]);
        table.extend_from_slice(&flags.to_le_bytes());
        table.extend_from_slice(&[0, irq, 0, pin]);
    }
    // An input past the ISA IRQs has no bus convention to conform to, so its
    // entry states its polarity and trigger. Linux takes the source IRQ of a
    // non-PCI bus, here the input's own number, as the interrupt's number.
    for pin in ISA_IRQ_COUNT..config.ioapic_inputs {
        table.extend_from_slice(&[3, 0]);
        table.extend_from_slice(&MP_IRQ_FLAGS_EDGE_HIGH.to_le_bytes());
        table.extend_from_slice(&[0, pin, 0, pin]);
    }
    assert_eq!(table.len(), capacity);

    let table_len = u16::try_from(table.len()).map_err(|_| Error::TooManyEntries)?;
    table[4..6].copy_from_slice(&table_len.to_le_bytes());
    table[7] = checksum(&table);
    Ok(table)
}

fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(
        bytes
            .iter()
            .copied()
            .fold(0u8, |sum, byte| sum.wrapping_add(byte)),
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
    fn interrupt_sources(table: &[u8], processor_count: usize) -> Vec<[u8; MP_ENTRY_SIZE]> {
        table[MP_CONFIG_HEADER_SIZE + MP_PROCESSOR_SIZE * processor_count..]
            .chunks_exact(MP_ENTRY_SIZE)
            .map(|entry| entry.try_into().unwrap())
            .filter(|entry: &[u8; MP_ENTRY_SIZE]| entry[0] == 3)
            .collect()
    }

    #[test]
    fn builds_valid_checksums_and_processor_entries() {
        for processor_count in [1usize, 2, 4, 8] {
            let apic_ids = (0..processor_count as u32).collect::<Vec<_>>();
            let tables = build(&MpTableConfig {
                apic_ids: &apic_ids,
                level_triggered_irqs: &[4, 5, 6, 7],
                ioapic_inputs: IOAPIC_INPUTS,
            })
            .unwrap();

            assert_eq!(&tables.floating_pointer[..4], b"_MP_");
            assert_eq!(
                u32::from_le_bytes(tables.floating_pointer[4..8].try_into().unwrap()),
                MP_CONFIG_TABLE_ADDR as u32
            );
            assert_eq!(
                tables
                    .floating_pointer
                    .iter()
                    .copied()
                    .fold(0u8, |sum, byte| sum.wrapping_add(byte)),
                0
            );
            assert_eq!(
                tables
                    .configuration_table
                    .iter()
                    .copied()
                    .fold(0u8, |sum, byte| sum.wrapping_add(byte)),
                0
            );
            // The header, the processors, the bus, the IOAPIC, and 23
            // interrupt sources: 15 ISA IRQs and inputs 16 through 23.
            assert_eq!(
                tables.configuration_table.len(),
                244 + MP_PROCESSOR_SIZE * processor_count
            );
            assert_eq!(
                u16::from_le_bytes(tables.configuration_table[34..36].try_into().unwrap()),
                processor_count as u16 + 25
            );
            for (index, entry) in tables.configuration_table
                [MP_CONFIG_HEADER_SIZE..MP_CONFIG_HEADER_SIZE + MP_PROCESSOR_SIZE * processor_count]
                .chunks_exact(MP_PROCESSOR_SIZE)
                .enumerate()
            {
                assert_eq!(entry[0], 0);
                assert_eq!(entry[1], index as u8);
                assert_eq!(entry[3], if index == 0 { 3 } else { 1 });
            }
        }
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
        // destination IOAPIC, and destination input. The PIT's IRQ 0 reaches
        // input 2, and IRQ 2, the PIC cascade, has no entry.
        let mut expected = vec![[3, 0, 0, 0, 0, 0, 0, 2]];
        for irq in 1..16 {
            if irq != 2 {
                let flags = if irq == 5 { 0x0d } else { 0 };
                expected.push([3, 0, flags, 0, 0, irq, 0, irq]);
            }
        }
        // The inputs past the ISA IRQs are active high and edge triggered.
        for input in 16..24 {
            expected.push([3, 0, 0x05, 0, 0, input, 0, input]);
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
        assert_eq!(table.len(), 180 + MP_PROCESSOR_SIZE);
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
        // Only the ISA IRQs may be level triggered.
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
