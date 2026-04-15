// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use inspect::Inspect;

/// Maximum non-contiguous ranges the assembler can track. Each range is a
/// "data island" separated by a gap (unreceived hole). 4 suffices for
/// typical TCP loss patterns. When the table is full, new non-mergeable
/// segments are dropped — the sender retransmits and gaps fill over time.
const MAX_RANGES: usize = 4;

/// The assembler's range table is full and the new segment does not overlap
/// or touch any existing range, so it cannot be tracked.
#[derive(Debug, PartialEq, Eq)]
pub struct TooManyGaps;

/// Result of an `add` call.
#[derive(Debug, PartialEq, Eq)]
pub struct AddResult {
    /// Number of contiguous data bytes consumed from the front (offset 0).
    /// The caller should `extend_by` this many bytes and advance `rx_seq`
    /// by this amount. 0 when the segment is purely out-of-order.
    pub consumed: u32,
    /// True if a FIN has been received and is now in-order — i.e., the
    /// contiguous frontier has reached the FIN's position. The caller
    /// should advance `rx_seq` by 1 (for the FIN's sequence-space byte)
    /// and transition the TCP state machine.
    pub fin: bool,
}

/// Tracks which byte ranges have been received out-of-order in the receive
/// buffer, and whether a FIN has been received.
///
/// All offsets are relative to the current *frontier* (`rx_seq`): offset 0
/// is the next expected in-order byte. An in-order segment has offset 0;
/// an out-of-order segment has offset > 0.
///
/// Internally stores up to `MAX_RANGES` sorted, non-overlapping,
/// non-adjacent `(start, end)` half-open ranges.
pub(super) struct Assembler {
    /// `ranges[..count]` are valid. Sorted by start.
    /// Invariant: `ranges[i].1 < ranges[i+1].0` for all `i < count - 1`.
    ranges: [(u32, u32); MAX_RANGES],
    count: u8,
    /// If a FIN has been received, this is `Some(offset)` where `offset` is
    /// the FIN's position relative to the current frontier. The FIN occupies
    /// zero bytes in the data stream but has a sequence-space position equal
    /// to the end of the sender's data. When the contiguous frontier reaches
    /// this offset, the FIN is "delivered" via `AddResult::fin`.
    fin_offset: Option<u32>,
}

impl Inspect for Assembler {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for (i, &(start, end)) in self.ranges[..self.count as usize].iter().enumerate() {
            resp.field(&format!("range_{i}"), format!("[{start}, {end})"));
        }
        if let Some(fo) = self.fin_offset {
            resp.field("fin_offset", fo);
        }
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            ranges: [(0, 0); MAX_RANGES],
            count: 0,
            fin_offset: None,
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Record that bytes `[offset, offset + len)` have been received.
    /// If `fin` is true, a FIN is present at offset `offset + len` (i.e.,
    /// immediately after this segment's data). The FIN is latched and will
    /// be delivered when the contiguous prefix reaches it.
    ///
    /// If this extends the contiguous prefix starting at offset 0, the prefix
    /// is consumed: the first range is removed and all remaining ranges are
    /// shifted left. `AddResult::consumed` contains the number of contiguous
    /// bytes consumed. `AddResult::fin` is true if the FIN is now in-order
    /// (all preceding data received).
    ///
    /// Returns `Err(TooManyGaps)` if the range table is full and the segment
    /// doesn't merge with any existing range. In that case the assembler is
    /// unchanged (the FIN is still latched if `fin` was true) and the caller
    /// should drop the segment (don't write it to the ring).
    pub fn add(&mut self, offset: u32, len: u32, fin: bool) -> Result<AddResult, TooManyGaps> {
        // Latch the FIN if present. The FIN sits at offset + len in
        // data-offset space. We record it *before* processing the data
        // range so that if this segment also completes the contiguous
        // prefix, the FIN is delivered in the same call.
        if fin {
            self.fin_offset = Some(offset + len);
        }

        if len == 0 {
            return Ok(self.try_fin(0));
        }

        let new_start = offset;
        let new_end = offset + len;

        // Find the range of existing entries that overlap or are adjacent to
        // the new range. Two half-open ranges [s,e) and [ns,ne)
        // overlap-or-touch when s <= ne && ns <= e.
        let ranges = &self.ranges[..self.count as usize];
        let first = ranges
            .iter()
            .position(|&(s, e)| s <= new_end && new_start <= e);
        let last = ranges
            .iter()
            .rposition(|&(s, e)| s <= new_end && new_start <= e);

        match (first, last) {
            (Some(first), Some(last)) => {
                // Merge new range with ranges[first..=last].
                let merged_start = new_start.min(self.ranges[first].0);
                let merged_end = new_end.max(self.ranges[last].1);
                self.ranges[first] = (merged_start, merged_end);
                // Remove the now-redundant entries (first+1..=last) by
                // shifting everything after `last` down.
                let remove = last - first;
                self.ranges
                    .copy_within(last + 1..self.count as usize, first + 1);
                self.count -= remove as u8;
            }
            (None, None) => {
                // No overlap: insert as a new entry.
                let count = self.count as usize;
                if count >= MAX_RANGES {
                    return Err(TooManyGaps);
                }
                let pos = ranges
                    .iter()
                    .position(|&(s, _)| s > new_start)
                    .unwrap_or(count);
                self.ranges.copy_within(pos..count, pos + 1);
                self.ranges[pos] = (new_start, new_end);
                self.count += 1;
            }
            _ => unreachable!("first.is_some() iff last.is_some()"),
        }

        // Consume the contiguous prefix. If ranges[0] starts at 0, those
        // bytes are ready — remove the range and shift coordinates.
        if self.count > 0 && self.ranges[0].0 == 0 {
            let front = self.ranges[0].1;
            let new_count = self.count as usize - 1;
            for i in 0..new_count {
                self.ranges[i] = (self.ranges[i + 1].0 - front, self.ranges[i + 1].1 - front);
            }
            self.count = new_count as u8;
            // Shift the FIN offset by the consumed amount.
            if let Some(ref mut fo) = self.fin_offset {
                *fo -= front;
            }
            Ok(self.try_fin(front))
        } else {
            Ok(self.try_fin(0))
        }
    }

    #[cfg(test)]
    pub fn clear(&mut self) {
        self.count = 0;
        self.fin_offset = None;
    }

    /// Check whether the latched FIN is now in-order and return an
    /// `AddResult`. If the FIN offset is 0 (frontier has reached it) and
    /// there are no remaining data ranges, the FIN is delivered: `fin_offset`
    /// is cleared and `AddResult::fin` is set to true.
    fn try_fin(&mut self, consumed: u32) -> AddResult {
        let fin = match self.fin_offset {
            Some(0) if self.count == 0 => {
                self.fin_offset = None;
                true
            }
            _ => false,
        };
        AddResult { consumed, fin }
    }

    #[cfg(test)]
    fn ranges(&self) -> &[(u32, u32)] {
        &self.ranges[..self.count as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty() {
        let a = Assembler::new();
        assert!(a.is_empty());
    }

    #[test]
    fn test_add_in_order() {
        let mut a = Assembler::new();
        let r = a.add(0, 10, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 10,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[]);
        assert!(a.is_empty());
    }

    #[test]
    fn test_add_single_ooo() {
        let mut a = Assembler::new();
        let r = a.add(10, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(10, 15)]);
    }

    #[test]
    fn test_fill_gap() {
        let mut a = Assembler::new();
        a.add(10, 5, false).unwrap();
        let r = a.add(0, 10, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 15,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[]);
    }

    #[test]
    fn test_fill_middle_gap() {
        let mut a = Assembler::new();
        let r = a.add(0, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 5,
                fin: false
            }
        );

        // The assembler was empty after the first consume, so [15,20) is
        // stored as-is (the shift only applies to ranges present at consume
        // time). The caller is responsible for recomputing offsets relative
        // to the new frontier.
        let r = a.add(15, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(15, 20)]);

        // add(5, 10) covers [5,15) which touches [15,20) → merge into [5,20).
        // Doesn't start at 0, so not consumed.
        let r = a.add(5, 10, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(5, 20)]);
    }

    #[test]
    fn test_fill_middle_gap_no_intervening_consume() {
        let mut a = Assembler::new();
        let r = a.add(10, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );

        let r = a.add(20, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );

        let r = a.add(0, 25, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 25,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[]);
    }

    #[test]
    fn test_adjacent_merge() {
        let mut a = Assembler::new();
        let r = a.add(5, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );

        let r = a.add(0, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 10,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[]);
    }

    #[test]
    fn test_overlap_extend() {
        let mut a = Assembler::new();
        // First: in-order, consumed immediately.
        let r = a.add(0, 10, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 10,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[]);

        // Overlapping in-order segment (retransmit + new data).
        let r = a.add(0, 15, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 15,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[]);

        // Now test overlap without immediate consumption.
        let r = a.add(5, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(5, 10)]);

        let r = a.add(3, 12, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(3, 15)]);
    }

    #[test]
    fn test_overlap_subset() {
        let mut a = Assembler::new();
        a.add(5, 15, false).unwrap();
        let r = a.add(8, 3, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(5, 20)]);
    }

    #[test]
    fn test_overlap_superset() {
        let mut a = Assembler::new();
        a.add(5, 3, false).unwrap();
        let r = a.add(3, 10, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(3, 13)]);
    }

    #[test]
    fn test_multiple_ooo_ranges() {
        let mut a = Assembler::new();
        let r = a.add(10, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        let r = a.add(20, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        let r = a.add(30, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(10, 15), (20, 25), (30, 35)]);
    }

    #[test]
    fn test_table_full_reject() {
        let mut a = Assembler::new();
        a.add(10, 1, false).unwrap();
        a.add(20, 1, false).unwrap();
        a.add(30, 1, false).unwrap();
        a.add(40, 1, false).unwrap();
        assert_eq!(a.ranges(), &[(10, 11), (20, 21), (30, 31), (40, 41)]);

        let r = a.add(50, 1, false);
        assert_eq!(r, Err(TooManyGaps));
        // State unchanged.
        assert_eq!(a.ranges(), &[(10, 11), (20, 21), (30, 31), (40, 41)]);
    }

    #[test]
    fn test_table_full_merge_ok() {
        let mut a = Assembler::new();
        a.add(10, 1, false).unwrap();
        a.add(20, 1, false).unwrap();
        a.add(30, 1, false).unwrap();
        a.add(40, 1, false).unwrap();

        // Merge with first range.
        let r = a.add(11, 9, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(10, 21), (30, 31), (40, 41)]);

        // Now there's room for a new range.
        let r = a.add(50, 1, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(10, 21), (30, 31), (40, 41), (50, 51)]);
    }

    #[test]
    fn test_merge_all() {
        let mut a = Assembler::new();
        a.add(4, 2, false).unwrap();
        a.add(8, 2, false).unwrap();
        a.add(12, 2, false).unwrap();
        a.add(16, 2, false).unwrap();
        assert_eq!(a.ranges(), &[(4, 6), (8, 10), (12, 14), (16, 18)]);

        let r = a.add(0, 18, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 18,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[]);
    }

    #[test]
    fn test_duplicate_segment() {
        let mut a = Assembler::new();
        let r = a.add(5, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        let r = a.add(5, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(5, 10)]);
    }

    #[test]
    fn test_consume_with_remaining() {
        let mut a = Assembler::new();
        let r = a.add(10, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.ranges(), &[(10, 15)]);

        let r = a.add(0, 8, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 8,
                fin: false
            }
        );
        // [10,15) shifted by 8 → [2,7).
        assert_eq!(a.ranges(), &[(2, 7)]);
    }

    #[test]
    fn test_add_zero_len() {
        let mut a = Assembler::new();
        let r = a.add(5, 0, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert!(a.is_empty());
    }

    #[test]
    fn test_clear() {
        let mut a = Assembler::new();
        a.add(10, 5, false).unwrap();
        a.add(20, 5, false).unwrap();
        a.clear();
        assert!(a.is_empty());
    }

    #[test]
    fn test_fin_in_order() {
        let mut a = Assembler::new();
        let r = a.add(0, 10, true).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 10,
                fin: true
            }
        );
        assert_eq!(a.ranges(), &[]);
        assert_eq!(a.fin_offset, None);
    }

    #[test]
    fn test_fin_ooo_then_fill() {
        let mut a = Assembler::new();
        let r = a.add(5, 5, true).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );

        let r = a.add(0, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 10,
                fin: true
            }
        );
        assert_eq!(a.ranges(), &[]);
    }

    #[test]
    fn test_fin_not_yet() {
        let mut a = Assembler::new();
        let r = a.add(10, 10, true).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.fin_offset, Some(20));

        // Partial fill — doesn't reach the FIN.
        let r = a.add(0, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 5,
                fin: false
            }
        );
        assert_eq!(a.fin_offset, Some(15));

        // Fill the rest.
        let r = a.add(0, 15, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 15,
                fin: true
            }
        );
        assert_eq!(a.ranges(), &[]);
        assert_eq!(a.fin_offset, None);
    }

    #[test]
    fn test_fin_pure_no_data() {
        let mut a = Assembler::new();
        let r = a.add(0, 0, true).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: true
            }
        );
        assert_eq!(a.fin_offset, None);
    }

    #[test]
    fn test_fin_with_gaps_remaining() {
        let mut a = Assembler::new();
        let r = a.add(10, 10, true).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: false
            }
        );
        assert_eq!(a.fin_offset, Some(20));

        let r = a.add(0, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 5,
                fin: false
            }
        );
        // [10,20) shifted by 5 → [5,15). fin_offset shifted to 15.
        assert_eq!(a.ranges(), &[(5, 15)]);
        assert_eq!(a.fin_offset, Some(15));

        // Fill the remaining gap [0,5) which merges with [5,15) → [0,15), consumed.
        // fin_offset 15 - 15 = 0, delivered.
        let r = a.add(0, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 15,
                fin: true
            }
        );
        assert_eq!(a.ranges(), &[]);
    }

    #[test]
    fn test_fin_on_data_segment_not_carried() {
        // This tests the correct pattern: the FIN arrives as a pure
        // zero-length segment after all data has been received.
        let mut a = Assembler::new();
        a.add(5, 5, false).unwrap();
        let r = a.add(0, 5, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 10,
                fin: false
            }
        );

        // Pure FIN arrives after all data.
        let r = a.add(0, 0, true).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 0,
                fin: true
            }
        );
        assert_eq!(a.fin_offset, None);
    }

    #[test]
    fn test_clear_clears_fin() {
        let mut a = Assembler::new();
        a.add(5, 5, true).unwrap();
        a.clear();
        let r = a.add(0, 10, false).unwrap();
        assert_eq!(
            r,
            AddResult {
                consumed: 10,
                fin: false
            }
        );
    }
}
