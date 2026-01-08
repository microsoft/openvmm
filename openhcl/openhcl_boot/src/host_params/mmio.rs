// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Manages MMIO range partitioning between VTLs.

use super::dt::DtError;
use memory_range::MemoryRange;

/// The start address of MMIO high range.
const MMIO_HIGH_RANGE_START: u64 = 1 << 32;

/// Select the mmio range that VTL2 should use from looking at VTL0 mmio
/// ranges.
///
/// VTL2 MMIO is partitioned such that:
/// - All MMIO low range is assigned to VTL0.
/// - VTL2_MMIO_HIGH_RANGE_SIZE bytes from the end of the high range is
///   assigned to VTL2.
/// - The remaining high range is assigned to VTL0.
///
/// Assumes input ranges are non-overlapping and in increasing address
/// order.
///
/// On success, returns the mmio that VTL2 should use.
///
/// Returns an error if the input VTL0 MMIO range is invalid or if the VTL2
/// allocation amount was not satisfied due to a lack of high MMIO assigned
/// to VTL0.
pub fn select_vtl2_mmio_range(
    mmio: &[MemoryRange],
    vtl2_size: u64,
) -> Result<MemoryRange, DtError> {
    // Iterate over the list of MMIO ranges in reverse address order so that
    // the VTL2 range is carved out from the end.
    for range in mmio.iter().rev() {
        // Do not select low MMIO ranges for VTL2.
        if range.start() < MMIO_HIGH_RANGE_START {
            continue;
        }

        // Compute the length of the VTL2 subrange. If there is not enough
        // mmio, give up.
        if range.len() < vtl2_size {
            return Err(DtError::NotEnoughMmio);
        }

        let vtl2_range_start = range.end() - vtl2_size;

        return Ok(MemoryRange::new(vtl2_range_start..range.end()));
    }

    Err(DtError::NotEnoughMmio)
}

#[cfg(test)]
mod test {
    use super::*;
    use arrayvec::ArrayVec;
    use memory_range::subtract_ranges;

    /// The size (in bytes) of MMIO high range assigned to VTL2.
    const VTL2_MMIO_HIGH_RANGE_SIZE: u64 = 128 * (1 << 20);

    // Tests that MMIO range is partitioned correctly between VTL0 and VTL2
    // for a variety of input ranges.
    #[test]
    fn mmio_range_partitioned_correctly() {
        #[derive(Debug)]
        struct TestCase {
            // Input
            mmio: ArrayVec<MemoryRange, 2>,
            // Expected output
            succeeds: bool,
            vtl0_range: ArrayVec<MemoryRange, 2>,
            vtl2_range: MemoryRange,
        }

        let testcases = [
            TestCase {
                // No MMIO range is provided, fails.
                mmio: ArrayVec::new(),
                succeeds: false,
                vtl0_range: ArrayVec::new(),
                vtl2_range: MemoryRange::EMPTY,
            },
            TestCase {
                // Only low mmio, fails.
                mmio: ArrayVec::from([
                    MemoryRange::new(0x3000_0000..0x4000_0000),
                    MemoryRange::new(0x4000_0000..0x5000_0000),
                ]),
                succeeds: false,
                vtl0_range: ArrayVec::new(),
                vtl2_range: MemoryRange::EMPTY,
            },
            TestCase {
                // MMIO high range is less than what VTL2 requested.
                mmio: ArrayVec::from([
                    MemoryRange::new(0x3000_0000..0x4000_0000),
                    MemoryRange::new(
                        MMIO_HIGH_RANGE_START
                            ..(MMIO_HIGH_RANGE_START + VTL2_MMIO_HIGH_RANGE_SIZE / 2),
                    ),
                ]),
                succeeds: false,
                vtl0_range: ArrayVec::new(),
                vtl2_range: MemoryRange::EMPTY,
            },
            TestCase {
                // MMIO high range is just enough for VTL2.
                // Low range should be assigned to VTL0.
                // High range should be assigned to VTL2.
                mmio: ArrayVec::from([
                    MemoryRange::new(0x3000_0000..0x4000_0000),
                    MemoryRange::new(
                        MMIO_HIGH_RANGE_START..(MMIO_HIGH_RANGE_START + VTL2_MMIO_HIGH_RANGE_SIZE),
                    ),
                ]),
                succeeds: true,
                vtl0_range: [MemoryRange::new(0x3000_0000..0x4000_0000)]
                    .into_iter()
                    .collect(),
                vtl2_range: MemoryRange::new(
                    MMIO_HIGH_RANGE_START..(MMIO_HIGH_RANGE_START + VTL2_MMIO_HIGH_RANGE_SIZE),
                ),
            },
            TestCase {
                // MMIO high range is more than what VTL2 requested.
                // VTL2 should be assigned SIZE from the end of the high range.
                // VTL0 should be assigned the remaining high range.
                mmio: ArrayVec::from([
                    MemoryRange::new(0x3000_0000..0x4000_0000),
                    MemoryRange::new(
                        MMIO_HIGH_RANGE_START
                            ..(MMIO_HIGH_RANGE_START + VTL2_MMIO_HIGH_RANGE_SIZE * 4),
                    ),
                ]),
                succeeds: true,
                vtl0_range: ArrayVec::from([
                    MemoryRange::new(0x3000_0000..0x4000_0000),
                    MemoryRange::new(
                        MMIO_HIGH_RANGE_START
                            ..(MMIO_HIGH_RANGE_START + VTL2_MMIO_HIGH_RANGE_SIZE * 3),
                    ),
                ]),
                vtl2_range: MemoryRange::new(
                    MMIO_HIGH_RANGE_START + VTL2_MMIO_HIGH_RANGE_SIZE * 3
                        ..(MMIO_HIGH_RANGE_START + VTL2_MMIO_HIGH_RANGE_SIZE * 4),
                ),
            },
            TestCase {
                // Multiple MMIO high ranges are provided.
                // VTL2 should be assigned SIZE from the very end of the high range.
                // VTL0 should be assigned all the remaining high ranges.
                mmio: ArrayVec::from([
                    MemoryRange::new(
                        MMIO_HIGH_RANGE_START
                            ..(MMIO_HIGH_RANGE_START + VTL2_MMIO_HIGH_RANGE_SIZE * 2),
                    ),
                    MemoryRange::new(
                        (MMIO_HIGH_RANGE_START * 2)
                            ..(MMIO_HIGH_RANGE_START * 2 + VTL2_MMIO_HIGH_RANGE_SIZE * 2),
                    ),
                ]),
                succeeds: true,
                vtl0_range: ArrayVec::from([
                    MemoryRange::new(
                        MMIO_HIGH_RANGE_START
                            ..(MMIO_HIGH_RANGE_START + VTL2_MMIO_HIGH_RANGE_SIZE * 2),
                    ),
                    MemoryRange::new(
                        (MMIO_HIGH_RANGE_START * 2)
                            ..(MMIO_HIGH_RANGE_START * 2 + VTL2_MMIO_HIGH_RANGE_SIZE),
                    ),
                ]),
                vtl2_range: MemoryRange::new(
                    (MMIO_HIGH_RANGE_START * 2 + VTL2_MMIO_HIGH_RANGE_SIZE)
                        ..(MMIO_HIGH_RANGE_START * 2 + VTL2_MMIO_HIGH_RANGE_SIZE * 2),
                ),
            },
        ];

        // Run all test cases.
        for (i, tc) in testcases.iter().enumerate() {
            let result = select_vtl2_mmio_range(&tc.mmio, VTL2_MMIO_HIGH_RANGE_SIZE);

            assert_eq!(
                tc.succeeds,
                result.is_ok(),
                "test case #{i}: unexpected result"
            );

            if tc.succeeds {
                let vtl2_mmio = result.unwrap();
                let vtl0_mmio =
                    subtract_ranges(tc.mmio.iter().cloned(), [vtl2_mmio]).collect::<Vec<_>>();

                assert_eq!(
                    tc.vtl0_range.as_slice(),
                    vtl0_mmio.as_slice(),
                    "test case #{i}: vtl0 was assigned an unexpected mmio range"
                );
                assert_eq!(
                    tc.vtl2_range, vtl2_mmio,
                    "test case #{i}: vtl1 was assigned an unexpected mmio range"
                );
            }
        }
    }
}
