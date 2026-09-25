// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel MP 1.4 table construction for x86 guests.

use thiserror::Error;

/// Guest-physical address of the MP floating pointer.
pub const MP_FLOATING_POINTER_ADDR: usize = 0;
/// Guest-physical address of the MP configuration table.
pub const MP_CONFIG_TABLE_ADDR: usize = 0x400;

const MP_IRQ_FLAGS_LEVEL_HIGH: u16 = 0x000d;
const MP_CONFIG_HEADER_SIZE: usize = 44;
const MP_PROCESSOR_SIZE: usize = 20;
const MP_NON_PROCESSOR_ENTRY_COUNT: usize = 17;

/// Guest-visible processor and interrupt data represented by MP tables.
#[derive(Debug, Clone, Copy)]
pub struct MpTableConfig<'a> {
    /// APIC IDs in virtual-processor order. The first entry is the BSP.
    pub apic_ids: &'a [u32],
    /// ISA IRQs described as active-high, level-triggered.
    pub level_triggered_irqs: &'a [u32],
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
    if let Some(&irq) = config.level_triggered_irqs.iter().find(|irq| **irq >= 16) {
        return Err(Error::InvalidIrq(irq));
    }
    let entry_count = u16::try_from(
        config
            .apic_ids
            .len()
            .checked_add(MP_NON_PROCESSOR_ENTRY_COUNT)
            .ok_or(Error::TooManyEntries)?,
    )
    .map_err(|_| Error::TooManyEntries)?;

    let capacity = MP_CONFIG_HEADER_SIZE
        .checked_add(
            MP_PROCESSOR_SIZE
                .checked_mul(config.apic_ids.len())
                .ok_or(Error::TooManyEntries)?,
        )
        .and_then(|size| size.checked_add(136))
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

    for irq in (0u8..16).filter(|irq| *irq != 2) {
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

    #[test]
    fn builds_valid_checksums_and_processor_entries() {
        for processor_count in [1usize, 2, 4, 8] {
            let apic_ids = (0..processor_count as u32).collect::<Vec<_>>();
            let tables = build(&MpTableConfig {
                apic_ids: &apic_ids,
                level_triggered_irqs: &[4, 5, 6, 7],
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
            assert_eq!(
                tables.configuration_table.len(),
                180 + MP_PROCESSOR_SIZE * processor_count
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
    fn rejects_invalid_inputs() {
        assert_eq!(
            build(&MpTableConfig {
                apic_ids: &[],
                level_triggered_irqs: &[],
            }),
            Err(Error::NoProcessors)
        );
        assert!(matches!(
            build(&MpTableConfig {
                apic_ids: &[0, 2],
                level_triggered_irqs: &[],
            }),
            Err(Error::InvalidApicId { .. })
        ));
        assert_eq!(
            build(&MpTableConfig {
                apic_ids: &[0],
                level_triggered_irqs: &[16],
            }),
            Err(Error::InvalidIrq(16))
        );
        let apic_ids = (0..=256).collect::<Vec<_>>();
        assert!(matches!(
            build(&MpTableConfig {
                apic_ids: &apic_ids,
                level_triggered_irqs: &[],
            }),
            Err(Error::ApicIdTooLarge {
                index: 256,
                apic_id: 256
            })
        ));
    }
}
