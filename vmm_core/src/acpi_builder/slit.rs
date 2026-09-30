// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use thiserror::Error;
use zerocopy::FromBytes;

/// NUMA distance information for SLIT generation.
#[derive(Debug)]
pub struct SlitInfo {
    /// Number of NUMA nodes (system localities).
    pub num_nodes: usize,
    /// Explicit distance entries (src, dst, distance).
    /// Entries not specified default to 10 (self) or 20 (cross-node).
    pub distances: Vec<(u32, u32, u8)>,
}

/// An error parsing an ACPI System Locality Information Table.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseSlitError {
    /// The table exceeds the caller's allocation limit.
    #[error("SLIT length {actual} exceeds limit {limit}")]
    TooLarge {
        /// The supplied length.
        actual: usize,
        /// The permitted length.
        limit: usize,
    },
    /// The table does not contain both fixed headers.
    #[error("SLIT header is truncated")]
    TruncatedHeader,
    /// The ACPI signature does not identify a SLIT.
    #[error("invalid SLIT signature {0:?}")]
    Signature([u8; 4]),
    /// The table revision is not supported.
    #[error("unsupported SLIT revision {0}")]
    Revision(u8),
    /// The supplied bytes do not match the ACPI length.
    #[error("SLIT header length {declared} does not match supplied length {actual}")]
    Length {
        /// The length from the header.
        declared: u32,
        /// The supplied length.
        actual: usize,
    },
    /// The table checksum is invalid.
    #[error("invalid SLIT checksum")]
    Checksum,
    /// The locality count is zero or cannot be represented safely.
    #[error("invalid SLIT locality count {0}")]
    Localities(u64),
    /// The locality count does not match the matrix length.
    #[error("SLIT matrix length {actual} does not match expected length {expected}")]
    MatrixLength {
        /// The required length.
        expected: usize,
        /// The supplied length.
        actual: usize,
    },
    /// A distance is reserved or a diagonal entry is not 10.
    #[error("invalid SLIT distance {src}->{dst}: {distance}")]
    Distance {
        /// The source locality.
        src: u32,
        /// The destination locality.
        dst: u32,
        /// The invalid distance.
        distance: u8,
    },
}

impl SlitInfo {
    /// Parses a revision-1 SLIT, bounded by `max_table_size` bytes.
    ///
    /// Checks the complete table before allocating distance overrides. Only
    /// distances differing from the generator's defaults are retained.
    pub fn parse(table: &[u8], max_table_size: usize) -> Result<Self, ParseSlitError> {
        if table.len() > max_table_size {
            return Err(ParseSlitError::TooLarge {
                actual: table.len(),
                limit: max_table_size,
            });
        }
        let (header, body) = acpi_spec::Header::read_from_prefix(table)
            .map_err(|_| ParseSlitError::TruncatedHeader)?;
        if header.signature != *b"SLIT" {
            return Err(ParseSlitError::Signature(header.signature));
        }
        if header.revision != acpi_spec::slit::SLIT_REVISION {
            return Err(ParseSlitError::Revision(header.revision));
        }
        if u64::from(header.length.get()) != table.len() as u64 {
            return Err(ParseSlitError::Length {
                declared: header.length.get(),
                actual: table.len(),
            });
        }
        if table.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte)) != 0 {
            return Err(ParseSlitError::Checksum);
        }
        let (slit, matrix) = acpi_spec::slit::SlitHeader::read_from_prefix(body)
            .map_err(|_| ParseSlitError::TruncatedHeader)?;
        let count = slit.number_of_system_localities.get();
        let invalid_count = || ParseSlitError::Localities(count);
        let num_nodes = usize::try_from(count).map_err(|_| invalid_count())?;
        if num_nodes == 0 || u32::try_from(count).is_err() {
            return Err(invalid_count());
        }
        let expected = num_nodes.checked_mul(num_nodes).ok_or_else(invalid_count)?;
        if matrix.len() != expected {
            return Err(ParseSlitError::MatrixLength {
                expected,
                actual: matrix.len(),
            });
        }

        let mut override_count = 0;
        for (index, &distance) in matrix.iter().enumerate() {
            let src = (index / num_nodes) as u32;
            let dst = (index % num_nodes) as u32;
            if distance < 10 || src == dst && distance != 10 {
                return Err(ParseSlitError::Distance { src, dst, distance });
            }
            if distance != if src == dst { 10 } else { 20 } {
                override_count += 1;
            }
        }

        let mut distances = Vec::with_capacity(override_count);
        for (index, &distance) in matrix.iter().enumerate() {
            let src = (index / num_nodes) as u32;
            let dst = (index % num_nodes) as u32;
            if distance != if src == dst { 10 } else { 20 } {
                distances.push((src, dst, distance));
            }
        }
        Ok(Self {
            num_nodes,
            distances,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi_builder::AcpiArchConfig;
    use crate::acpi_builder::AcpiTablesBuilder;
    use memory_range::MemoryRange;
    use test_with_tracing::test;
    use vm_topology::memory::MemoryLayout;
    use vm_topology::processor::TopologyBuilder;

    const LIMIT: usize = 20 * 4096;

    fn checksum(table: &mut [u8]) {
        table[9] = 0;
        table[9] = 0u8.wrapping_sub(table.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte)));
    }

    fn fixture(count: u64, matrix: &[u8]) -> Vec<u8> {
        let mut table = vec![0; 44];
        table[..4].copy_from_slice(b"SLIT");
        table[8] = 1;
        table[10..16].copy_from_slice(b"HOST  ");
        table[36..44].copy_from_slice(&count.to_le_bytes());
        table.extend_from_slice(matrix);
        let len = table.len() as u32;
        table[4..8].copy_from_slice(&len.to_le_bytes());
        checksum(&mut table);
        table
    }

    fn build(info: &SlitInfo) -> Vec<u8> {
        let topology = TopologyBuilder::new_x86().build(1).unwrap();
        let memory =
            MemoryLayout::new(4096, &[MemoryRange::new(4096..8192)], &[], &[], None).unwrap();
        AcpiTablesBuilder {
            processor_topology: &topology,
            mem_layout: &memory,
            cache_topology: None,
            pcie_host_bridges: &vec![],
            slit_info: Some(info),
            generic_initiators: &[],
            arch: AcpiArchConfig::X86 {
                with_ioapic: false,
                with_pic: false,
                with_pit: false,
                with_psp: false,
                pm_base: 0,
                acpi_irq: 0,
                iommu: None,
            },
        }
        .build_slit()
        .unwrap()
    }

    #[test]
    fn preserves_matrix() {
        for (count, matrix, overrides) in [
            (1, vec![10], vec![]),
            (2, vec![10, 20, 20, 10], vec![]),
            (2, vec![10, 17, 29, 10], vec![(0, 1, 17), (1, 0, 29)]),
            (2, vec![10, 10, 255, 10], vec![(0, 1, 10), (1, 0, 255)]),
        ] {
            let input = fixture(count, &matrix);
            let info = SlitInfo::parse(&input, LIMIT).unwrap();
            assert_eq!(info.num_nodes, count as usize);
            assert_eq!(info.distances, overrides);
            let output = build(&info);
            assert_eq!(&output[..4], b"SLIT");
            assert_eq!(output[8], 1);
            assert_eq!(output.len(), 44 + matrix.len());
            assert_eq!(&output[36..44], &count.to_le_bytes());
            assert_eq!(&output[44..], matrix);
            assert_ne!(&output[10..16], b"HOST  ");
            assert_eq!(output.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)), 0);
            assert_eq!(
                SlitInfo::parse(&output, LIMIT).unwrap().distances,
                overrides
            );
        }
    }

    #[test]
    fn rejects_invalid_envelopes() {
        let valid = fixture(1, &[10]);
        for end in 0..36 {
            assert_eq!(
                SlitInfo::parse(&valid[..end], LIMIT).unwrap_err(),
                ParseSlitError::TruncatedHeader
            );
        }
        let mut table = valid.clone();
        table[..4].copy_from_slice(b"PPTT");
        assert!(matches!(
            SlitInfo::parse(&table, LIMIT),
            Err(ParseSlitError::Signature(_))
        ));
        table = valid.clone();
        table[8] = 2;
        assert_eq!(
            SlitInfo::parse(&table, LIMIT).unwrap_err(),
            ParseSlitError::Revision(2)
        );
        table = valid.clone();
        table[4..8].copy_from_slice(&44u32.to_le_bytes());
        assert!(matches!(
            SlitInfo::parse(&table, LIMIT),
            Err(ParseSlitError::Length { .. })
        ));
        table = valid;
        table[9] = table[9].wrapping_add(1);
        assert_eq!(
            SlitInfo::parse(&table, LIMIT).unwrap_err(),
            ParseSlitError::Checksum
        );
        assert_eq!(
            SlitInfo::parse(&fixture(0, &[]), LIMIT).unwrap_err(),
            ParseSlitError::Localities(0)
        );
        assert_eq!(
            SlitInfo::parse(&fixture(u64::MAX, &[]), LIMIT).unwrap_err(),
            ParseSlitError::Localities(u64::MAX)
        );
        for count in [2, u32::MAX as u64] {
            assert!(matches!(
                SlitInfo::parse(&fixture(count, &[10]), LIMIT),
                Err(ParseSlitError::MatrixLength { .. }) | Err(ParseSlitError::Localities(_))
            ));
        }
        assert!(matches!(
            SlitInfo::parse(&fixture(1, &[10, 20]), LIMIT),
            Err(ParseSlitError::MatrixLength { .. })
        ));
        let mut truncated = fixture(1, &[]);
        truncated.truncate(43);
        truncated[4..8].copy_from_slice(&43u32.to_le_bytes());
        checksum(&mut truncated);
        assert_eq!(
            SlitInfo::parse(&truncated, LIMIT).unwrap_err(),
            ParseSlitError::TruncatedHeader
        );
    }

    #[test]
    fn rejects_invalid_distances() {
        for matrix in [
            [9, 20, 20, 10],
            [11, 20, 20, 10],
            [10, 0, 20, 10],
            [10, 20, 20, 255],
        ] {
            assert!(matches!(
                SlitInfo::parse(&fixture(2, &matrix), LIMIT),
                Err(ParseSlitError::Distance { .. })
            ));
        }
    }

    #[test]
    fn allocation_limit_and_owned_overrides() {
        let count = 286;
        let mut matrix = vec![255; count * count];
        for node in 0..count {
            matrix[node * count + node] = 10;
        }
        let mut table = fixture(count as u64, &matrix);
        let info = SlitInfo::parse(&table, table.len()).unwrap();
        assert_eq!(info.distances.len(), count * (count - 1));
        assert!(matches!(
            SlitInfo::parse(&table, table.len() - 1),
            Err(ParseSlitError::TooLarge { .. })
        ));
        table.fill(0);
        assert_eq!(info.distances[0], (0, 1, 255));
        assert_eq!(&build(&info)[44..], matrix);
        assert!(matches!(
            SlitInfo::parse(&vec![0; LIMIT + 1], LIMIT),
            Err(ParseSlitError::TooLarge { .. })
        ));
    }
}
