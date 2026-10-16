// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SLIT table parsing.
//! See [ACPI 6.5 section 5.2.17](https://uefi.org/specs/ACPI/6.5/05_ACPI_Software_Programming_Model.html#system-locality-information-table-slit).

use alloc::vec::Vec;
use thiserror::Error;
use zerocopy::FromBytes;

/// Validated SLIT localities and their row-major distance matrix.
#[derive(Debug)]
pub struct Slit {
    matrix: Vec<u8>,
    num_nodes: usize,
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

impl Slit {
    /// Parses a SLIT into owned locality and distance data.
    ///
    /// Supports table revision 1. Rejects tables larger than `max_table_size` bytes.
    /// Checks the signature, length, checksum, locality count, and matrix size.
    /// Each locality's distance to itself must be 10. All distances must be at least 10.
    ///
    /// This checks that the contents of the table are valid, but does not validate
    /// the described topology.
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

        for (index, &distance) in matrix.iter().enumerate() {
            let src = (index / num_nodes) as u32;
            let dst = (index % num_nodes) as u32;
            if distance < 10 || src == dst && distance != 10 {
                return Err(ParseSlitError::Distance { src, dst, distance });
            }
        }

        Ok(Self {
            matrix: matrix.to_vec(),
            num_nodes,
        })
    }

    /// Returns the number of system localities.
    pub fn num_nodes(&self) -> usize {
        self.num_nodes
    }

    /// Iterates over row-major overrides of the defaults (self 10, cross 20).
    ///
    /// The iterator scans the validated matrix without allocating.
    pub fn distances(&self) -> impl Iterator<Item = (u32, u32, u8)> + '_ {
        self.matrix
            .iter()
            .enumerate()
            .filter_map(|(index, &distance)| {
                let src = (index / self.num_nodes) as u32;
                let dst = (index % self.num_nodes) as u32;
                (distance != if src == dst { 10 } else { 20 }).then_some((src, dst, distance))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use test_with_tracing::test;

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

    #[test]
    fn parses_localities_and_overrides() {
        for (count, matrix, overrides) in [
            (1, vec![10], vec![]),
            (2, vec![10, 20, 20, 10], vec![]),
            (2, vec![10, 17, 29, 10], vec![(0, 1, 17), (1, 0, 29)]),
            (2, vec![10, 10, 255, 10], vec![(0, 1, 10), (1, 0, 255)]),
        ] {
            let input = fixture(count, &matrix);
            let slit = Slit::parse(&input, LIMIT).unwrap();
            drop(input);
            assert_eq!(slit.matrix, matrix);
            assert_eq!(slit.num_nodes(), count as usize);
            assert_eq!(slit.distances().collect::<Vec<_>>(), overrides);
        }
    }

    #[test]
    fn rejects_invalid_envelopes() {
        let valid = fixture(1, &[10]);
        for end in 0..36 {
            assert_eq!(
                Slit::parse(&valid[..end], LIMIT).unwrap_err(),
                ParseSlitError::TruncatedHeader
            );
        }
        let mut table = valid.clone();
        table[..4].copy_from_slice(b"PPTT");
        assert!(matches!(
            Slit::parse(&table, LIMIT),
            Err(ParseSlitError::Signature(_))
        ));
        table = valid.clone();
        table[8] = 2;
        assert_eq!(
            Slit::parse(&table, LIMIT).unwrap_err(),
            ParseSlitError::Revision(2)
        );
        table = valid.clone();
        table[4..8].copy_from_slice(&44u32.to_le_bytes());
        assert!(matches!(
            Slit::parse(&table, LIMIT),
            Err(ParseSlitError::Length { .. })
        ));
        table = valid;
        table[9] = table[9].wrapping_add(1);
        assert_eq!(
            Slit::parse(&table, LIMIT).unwrap_err(),
            ParseSlitError::Checksum
        );
        assert_eq!(
            Slit::parse(&fixture(0, &[]), LIMIT).unwrap_err(),
            ParseSlitError::Localities(0)
        );
        assert_eq!(
            Slit::parse(&fixture(u64::MAX, &[]), LIMIT).unwrap_err(),
            ParseSlitError::Localities(u64::MAX)
        );
        for count in [2, u32::MAX as u64] {
            assert!(matches!(
                Slit::parse(&fixture(count, &[10]), LIMIT),
                Err(ParseSlitError::MatrixLength { .. }) | Err(ParseSlitError::Localities(_))
            ));
        }
        assert!(matches!(
            Slit::parse(&fixture(1, &[10, 20]), LIMIT),
            Err(ParseSlitError::MatrixLength { .. })
        ));
        for end in 36..44 {
            let mut truncated = fixture(1, &[]);
            truncated.truncate(end);
            truncated[4..8].copy_from_slice(&(end as u32).to_le_bytes());
            checksum(&mut truncated);
            assert_eq!(
                Slit::parse(&truncated, LIMIT).unwrap_err(),
                ParseSlitError::TruncatedHeader
            );
        }
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
                Slit::parse(&fixture(2, &matrix), LIMIT),
                Err(ParseSlitError::Distance { .. })
            ));
        }
    }

    #[test]
    fn allocation_limit() {
        let count = 286;
        let mut matrix = vec![255; count * count];
        for node in 0..count {
            matrix[node * count + node] = 10;
        }
        let table = fixture(count as u64, &matrix);
        let len = table.len();
        assert!(matches!(
            Slit::parse(&table, len - 1),
            Err(ParseSlitError::TooLarge { .. })
        ));
        let slit = Slit::parse(&table, len).unwrap();
        assert_eq!(slit.distances().count(), count * (count - 1));
        assert!(matches!(
            Slit::parse(&vec![0; LIMIT + 1], LIMIT),
            Err(ParseSlitError::TooLarge { .. })
        ));
    }
}
