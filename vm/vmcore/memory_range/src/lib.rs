// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The [`MemoryRange`] type, which represents a 4KB-page-aligned byte range of
//! memory, plus algorithms operating on the type.

#![forbid(unsafe_code)]
#![no_std]

use core::iter::Iterator;
use core::iter::Peekable;
use core::ops::Range;

const PAGE_SIZE: u64 = 4096;
const TWO_MB: u64 = 0x20_0000;
const ONE_GB: u64 = 0x4000_0000;

/// Represents a page-aligned byte range of memory.
///
/// This type has a stable `Protobuf` representation, and can be directly used
/// in saved state.
// TODO: enforce invariants during de/serialization
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(
    feature = "mesh",
    derive(mesh_protobuf::Protobuf),
    mesh(package = "topology")
)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect), inspect(display))]
pub struct MemoryRange {
    #[cfg_attr(feature = "mesh", mesh(1))]
    start: u64,
    #[cfg_attr(feature = "mesh", mesh(2))]
    end: u64,
}

impl core::fmt::Display for MemoryRange {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#x}-{:#x}", self.start(), self.end())
    }
}

impl TryFrom<Range<u64>> for MemoryRange {
    type Error = InvalidMemoryRange;

    fn try_from(range: Range<u64>) -> Result<Self, Self::Error> {
        Self::try_new(range)
    }
}

impl TryFrom<Range<usize>> for MemoryRange {
    type Error = InvalidMemoryRange;

    fn try_from(range: Range<usize>) -> Result<Self, Self::Error> {
        Self::try_new(range.start as u64..range.end as u64)
    }
}

/// Error returned by [`MemoryRange::try_new`].
#[derive(Debug, thiserror::Error)]
#[error("unaligned or invalid memory range: {start:#x}-{end:#x}")]
pub struct InvalidMemoryRange {
    start: u64,
    end: u64,
}

impl MemoryRange {
    /// The maximum address that can be represented by a `MemoryRange`.
    pub const MAX_ADDRESS: u64 = !(PAGE_SIZE - 1);

    /// Returns a new range for the given guest address range.
    ///
    /// Panics if the start or end are not 4KB aligned or if the start is after
    /// the end.
    #[track_caller]
    pub const fn new(range: Range<u64>) -> Self {
        assert!(range.start & (PAGE_SIZE - 1) == 0);
        assert!(range.end & (PAGE_SIZE - 1) == 0);
        assert!(range.start <= range.end);
        Self {
            start: range.start,
            end: range.end,
        }
    }

    /// Returns a new range for the given guest address range.
    ///
    /// Returns `None` if the start or end are not 4KB aligned or if the start
    /// is after the end.
    pub const fn try_new(range: Range<u64>) -> Result<Self, InvalidMemoryRange> {
        if range.start & (PAGE_SIZE - 1) != 0
            || range.end & (PAGE_SIZE - 1) != 0
            || range.start > range.end
        {
            return Err(InvalidMemoryRange {
                start: range.start,
                end: range.end,
            });
        }
        Ok(Self {
            start: range.start,
            end: range.end,
        })
    }

    /// Returns the smallest 4K-aligned range that contains the given address
    /// range.
    ///
    /// Panics if the start is after the end or if the end address is in the
    /// last page of the 64-bit space.
    pub fn bounding(range: Range<u64>) -> Self {
        assert!(range.start <= range.end);
        assert!(range.end < u64::MAX - PAGE_SIZE);
        let start = range.start & !(PAGE_SIZE - 1);
        let end = (range.end + (PAGE_SIZE - 1)) & !(PAGE_SIZE - 1);
        Self::new(start..end)
    }

    /// Returns a new range for the given guest 4KB page range.
    ///
    /// Panics if the start is after the end or if the start address or end
    /// address overflow.
    pub fn from_4k_gpn_range(range: Range<u64>) -> Self {
        const MAX: u64 = u64::MAX / PAGE_SIZE;
        assert!(range.start <= MAX);
        assert!(range.end <= MAX);
        Self::new(range.start * PAGE_SIZE..range.end * PAGE_SIZE)
    }

    /// The empty range, with start and end addresses of zero.
    pub const EMPTY: Self = Self::new(0..0);

    /// The start address.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// The start address as a 4KB page number.
    pub fn start_4k_gpn(&self) -> u64 {
        self.start / PAGE_SIZE
    }

    /// The end address as a 4KB page number.
    pub fn end_4k_gpn(&self) -> u64 {
        self.end / PAGE_SIZE
    }

    /// The number of 4KB pages in the range.
    pub fn page_count_4k(&self) -> u64 {
        (self.end - self.start) / PAGE_SIZE
    }

    /// The number of 2MB pages in the range.
    pub fn page_count_2m(&self) -> u64 {
        (self.end - self.start).div_ceil(TWO_MB)
    }

    /// The end address.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// The length of the range in bytes.
    pub fn len(&self) -> u64 {
        self.end() - self.start()
    }

    /// Check if the range is empty.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Gets the biggest page size possible for the range.
    pub fn alignment(&self, base: u64) -> u64 {
        let order = ((base + self.start()) | (base + self.end())).trailing_zeros();
        1 << order
    }

    /// Returns the largest range contained in this range whose start and end
    /// are aligned to `alignment` bytes. This may be the empty range.
    ///
    /// Panics if `alignment` is not a power of two.
    pub fn aligned_subrange(&self, alignment: u64) -> Self {
        assert!(alignment.is_power_of_two());
        let start = (self.start + alignment - 1) & !(alignment - 1);
        let end = self.end & !(alignment - 1);
        if start <= end {
            Self::new(start..end)
        } else {
            Self::EMPTY
        }
    }

    /// Returns whether `self` and `other` overlap.
    pub fn overlaps(&self, other: &Self) -> bool {
        self.end > other.start && self.start < other.end
    }

    /// Returns whether `self` contains `other`.
    pub fn contains(&self, other: &Self) -> bool {
        self.start <= other.start && self.end >= other.end
    }

    /// Returns whether `self` contains the byte at `addr`.
    pub fn contains_addr(&self, addr: u64) -> bool {
        (self.start..self.end).contains(&addr)
    }

    /// Returns the byte offset of `addr` within the range, if it is contained.
    pub fn offset_of(&self, addr: u64) -> Option<u64> {
        if self.contains_addr(addr) {
            Some(addr - self.start)
        } else {
            None
        }
    }

    /// Returns the intersection of `self` and `other`.
    pub fn intersection(&self, other: &Self) -> Self {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        if start <= end {
            Self::new(start..end)
        } else {
            Self::EMPTY
        }
    }

    /// Split the range at the given byte offset within the range.
    ///
    /// Panics if `offset` is not within the range or is not page-aligned.
    #[track_caller]
    pub fn split_at_offset(&self, offset: u64) -> (Self, Self) {
        assert!(offset <= self.len());
        assert!(offset.is_multiple_of(PAGE_SIZE));
        (
            Self {
                start: self.start,
                end: self.start + offset,
            },
            Self {
                start: self.start + offset,
                end: self.end,
            },
        )
    }
}

impl From<MemoryRange> for Range<u64> {
    fn from(range: MemoryRange) -> Self {
        Range {
            start: range.start(),
            end: range.end(),
        }
    }
}

/// Iterator over aligned subranges of a memory range.
///
/// Each subrange will be made up of aligned pages of size 4KB, 2MB, or 1GB.
/// Subrange boundaries are chosen to output the maximum number of the largest
/// pages.
#[derive(Debug, Clone)]
pub struct AlignedSubranges {
    range: MemoryRange,
    offset: u64,
    max_len: u64,
}

impl AlignedSubranges {
    /// Returns a new iterator of subranges in `range`.
    pub fn new(range: MemoryRange) -> Self {
        Self {
            range,
            offset: 0,
            max_len: u64::MAX,
        }
    }

    /// Returns an iterator that considers subrange alignment offset by `offset`
    /// bytes.
    pub fn with_offset(self, offset: u64) -> Self {
        Self { offset, ..self }
    }

    /// Returns an iterator that outputs subranges only up to `max_len` bytes.
    pub fn with_max_range_len(self, max_len: u64) -> Self {
        Self { max_len, ..self }
    }
}

impl Iterator for AlignedSubranges {
    type Item = MemoryRange;

    fn next(&mut self) -> Option<Self::Item> {
        if self.range.is_empty() {
            return None;
        }

        let start = self.range.start() + self.offset;
        let mut end = self.range.end() + self.offset;
        if end - start > self.max_len {
            end = start + self.max_len;
        }

        let mut align = |page_size| {
            let page_mask: u64 = page_size - 1;
            if (start + page_mask) & !page_mask >= end & !page_mask {
                // No sense in aligning this, since we won't get any aligned
                // pages.
                return;
            }
            if start & page_mask != 0 {
                // Align the next range's start.
                end = end.min((start + page_mask) & !page_mask);
            } else {
                // Align this range's end.
                end &= !page_mask;
            }
        };

        align(TWO_MB);
        align(ONE_GB);
        let start = start - self.offset;
        let end = end - self.offset;
        self.range = MemoryRange::new(end..self.range.end());
        Some(MemoryRange::new(start..end))
    }
}

/// Returns an iterator over memory ranges that are in both `left` and `right`.
///
/// For example, if `left` is `[0..4MB, 8MB..12MB]` and `right` is `[2MB..6MB, 10MB..11MB]`,
/// the resulting iterator will yield `[2MB..4MB, 10MB..11MB]`.
///
/// Panics if `left` or `right` are not sorted or are overlapping.
pub fn overlapping_ranges(
    left: impl IntoIterator<Item = MemoryRange>,
    right: impl IntoIterator<Item = MemoryRange>,
) -> impl Iterator<Item = MemoryRange> {
    walk_ranges(
        left.into_iter().map(|r| (r, ())),
        right.into_iter().map(|r| (r, ())),
    )
    .filter_map(|(r, c)| match c {
        RangeWalkResult::Both((), ()) => Some(r),
        _ => None,
    })
}

/// Returns an iterator over the ranges in `left` that are not in `right`.
///
/// For example, if `left` is `[0..4MB, 8MB..12MB]` and `right` is `[2MB..6MB,
/// 10MB..11MB]`, the resulting iterator will yield `[0..2MB, 8MB..10MB,
/// 11MB..12MB]`.
///
/// Panics if `left` or `right` are not sorted or are overlapping.
pub fn subtract_ranges(
    left: impl IntoIterator<Item = MemoryRange>,
    right: impl IntoIterator<Item = MemoryRange>,
) -> impl Iterator<Item = MemoryRange> {
    walk_ranges(
        left.into_iter().map(|r| (r, ())),
        right.into_iter().map(|r| (r, ())),
    )
    .filter_map(|(r, c)| match c {
        RangeWalkResult::Left(()) => Some(r),
        RangeWalkResult::Neither | RangeWalkResult::Right(()) | RangeWalkResult::Both((), ()) => {
            None
        }
    })
}

/// Returns an iterator that computes the overlapping state of the ranges in
/// `left` and `right`.
///
/// The iterator yields a tuple of a [`MemoryRange`] and a [`RangeWalkResult`]
/// enum that indicates whether each subrange is only in `left`, only in
/// `right`, in both, or in neither.
///
/// Panics if `left` or `right` are not sorted.
///
/// # Examples
///
/// ```
/// # use memory_range::{MemoryRange, RangeWalkResult, walk_ranges};
/// let left = [(MemoryRange::new(0x100000..0x400000), "first"), (MemoryRange::new(0x800000..0xc00000), "second")];
/// let right = [(MemoryRange::new(0x200000..0x900000), 1000), (MemoryRange::new(0x900000..0xa00000), 2000)];
/// let v: Vec<_> = walk_ranges(left, right).collect();
/// let expected = [
///     (MemoryRange::new(0..0x100000), RangeWalkResult::Neither),
///     (MemoryRange::new(0x100000..0x200000), RangeWalkResult::Left("first")),
///     (MemoryRange::new(0x200000..0x400000), RangeWalkResult::Both("first", 1000)),
///     (MemoryRange::new(0x400000..0x800000), RangeWalkResult::Right(1000)),
///     (MemoryRange::new(0x800000..0x900000), RangeWalkResult::Both("second", 1000)),
///     (MemoryRange::new(0x900000..0xa00000), RangeWalkResult::Both("second", 2000)),
///     (MemoryRange::new(0xa00000..0xc00000), RangeWalkResult::Left("second")),
///     (MemoryRange::new(0xc00000..MemoryRange::MAX_ADDRESS), RangeWalkResult::Neither),
/// ];
/// assert_eq!(v.as_slice(), expected.as_slice());
/// ```
pub fn walk_ranges<T: Clone, U: Clone>(
    left: impl IntoIterator<Item = (MemoryRange, T)>,
    right: impl IntoIterator<Item = (MemoryRange, U)>,
) -> impl Iterator<Item = (MemoryRange, RangeWalkResult<T, U>)> {
    RangeWalkIter {
        pos: 0,
        left: PeekableSorted::new(left),
        right: PeekableSorted::new(right),
    }
}

/// The result of an iteration of [`walk_ranges`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RangeWalkResult<T, U> {
    /// Neither iterator contains this range.
    Neither,
    /// Only the left iterator contains this range, in the element with the
    /// given value.
    Left(T),
    /// Only the right iterator contains this range, in the element with the
    /// given value.
    Right(U),
    /// Both iterators contain this range, in the elements with the given
    /// values.
    Both(T, U),
}

struct RangeWalkIter<I: Iterator, J: Iterator> {
    pos: u64,
    left: PeekableSorted<I>,
    right: PeekableSorted<J>,
}

struct PeekableSorted<I: Iterator> {
    iter: I,
    #[expect(clippy::option_option)] // `Some(None)` is used to remember that `iter` is empty.
    item: Option<Option<I::Item>>,
}

impl<I: Iterator<Item = (MemoryRange, T)>, T> PeekableSorted<I> {
    fn new(iter: impl IntoIterator<IntoIter = I>) -> Self {
        Self {
            iter: iter.into_iter(),
            item: None,
        }
    }

    fn peek_in_range_ensure_sorted(&mut self, pos: u64, msg: &str) -> Option<&(MemoryRange, T)> {
        loop {
            let r = self
                .item
                .get_or_insert_with(|| {
                    let r = self.iter.next()?;
                    assert!(r.0.start() >= pos, "{msg} not sorted");
                    Some(r)
                })
                .as_ref()?;
            if !r.0.is_empty() && r.0.end() > pos {
                return Some(self.item.as_ref().unwrap().as_ref().unwrap());
            }
            self.item = None;
        }
    }
}

impl<I: Iterator<Item = (MemoryRange, T)>, J: Iterator<Item = (MemoryRange, U)>, T: Clone, U: Clone>
    Iterator for RangeWalkIter<I, J>
{
    type Item = (MemoryRange, RangeWalkResult<T, U>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos == MemoryRange::MAX_ADDRESS {
            return None;
        }
        let left = self.left.peek_in_range_ensure_sorted(self.pos, "left");
        let right = self.right.peek_in_range_ensure_sorted(self.pos, "right");
        let (end, c) = match (left, right) {
            (Some(&(left, ref t)), Some(&(right, ref u))) => {
                if self.pos < left.start() {
                    if self.pos < right.start() {
                        (left.start().min(right.start()), RangeWalkResult::Neither)
                    } else {
                        (
                            left.start().min(right.end()),
                            RangeWalkResult::Right(u.clone()),
                        )
                    }
                } else if self.pos < right.start() {
                    (
                        right.start().min(left.end()),
                        RangeWalkResult::Left(t.clone()),
                    )
                } else {
                    (
                        left.end().min(right.end()),
                        RangeWalkResult::Both(t.clone(), u.clone()),
                    )
                }
            }
            (Some(&(left, ref t)), None) => {
                if self.pos < left.start() {
                    (left.start, RangeWalkResult::Neither)
                } else {
                    (left.end(), RangeWalkResult::Left(t.clone()))
                }
            }
            (None, Some(&(right, ref u))) => {
                if self.pos < right.start() {
                    (right.start, RangeWalkResult::Neither)
                } else {
                    (right.end(), RangeWalkResult::Right(u.clone()))
                }
            }
            (None, None) => (MemoryRange::MAX_ADDRESS, RangeWalkResult::Neither),
        };
        let r = MemoryRange::new(self.pos..end);
        self.pos = end;
        Some((r, c))
    }
}

/// Takes a sequence of memory ranges, sorted by their start address, and
/// returns an iterator over the flattened ranges, where overlapping and
/// adjacent ranges are merged and deduplicated.
///
/// Panics if the input ranges are not sorted by their start address.
///
/// # Example
/// ```rust
/// # use memory_range::{flatten_ranges, MemoryRange};
/// let ranges = [
///     MemoryRange::new(0x1000..0x2000),
///     MemoryRange::new(0x2000..0x5000),
///     MemoryRange::new(0x4000..0x6000),
///     MemoryRange::new(0x5000..0x6000),
///     MemoryRange::new(0x8000..0x9000),
/// ];
/// let flattened = [
///     MemoryRange::new(0x1000..0x6000),
///     MemoryRange::new(0x8000..0x9000),
/// ];
/// assert!(flatten_ranges(ranges).eq(flattened));
/// ```
pub fn flatten_ranges(
    ranges: impl IntoIterator<Item = MemoryRange>,
) -> impl Iterator<Item = MemoryRange> {
    FlattenIter {
        iter: ranges.into_iter().peekable(),
    }
}

struct FlattenIter<I: Iterator> {
    iter: Peekable<I>,
}

impl<I: Iterator<Item = MemoryRange>> Iterator for FlattenIter<I> {
    type Item = MemoryRange;

    fn next(&mut self) -> Option<Self::Item> {
        let first = self.iter.next()?;
        let mut start = first.start();
        let mut end = first.end();
        while let Some(r) = self.iter.next_if(|r| {
            assert!(r.start() >= start, "ranges are not sorted");
            r.start() <= end
        }) {
            start = r.start();
            end = end.max(r.end());
        }
        Some(MemoryRange::new(first.start()..end))
    }
}

/// Similar to [`flatten_ranges`], but considers ranges non-equivalent if their
/// associated tags differ.
///
/// Panics if the input ranges are not sorted by their start address, or if
/// ranges overlap.
///
/// # Example
/// ```rust
/// # use memory_range::{merge_adjacent_ranges, MemoryRange};
///
/// #[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// enum Color {
///    Red,
///    Blue,
/// }
///
/// let ranges = [
///     (MemoryRange::new(0x1000..0x2000), Color::Red),
///     (MemoryRange::new(0x2000..0x5000), Color::Red),
///     (MemoryRange::new(0x5000..0x6000), Color::Blue),
///     (MemoryRange::new(0x8000..0x9000), Color::Red),
/// ];
/// let flattened = [
///     (MemoryRange::new(0x1000..0x5000), Color::Red),
///     (MemoryRange::new(0x5000..0x6000), Color::Blue),
///     (MemoryRange::new(0x8000..0x9000), Color::Red),
/// ];
/// assert!(merge_adjacent_ranges(ranges).eq(flattened));
/// ```
pub fn merge_adjacent_ranges<T: PartialEq>(
    ranges: impl IntoIterator<Item = (MemoryRange, T)>,
) -> impl Iterator<Item = (MemoryRange, T)> {
    MergeAdjacentIter {
        iter: ranges.into_iter().peekable(),
    }
}

struct MergeAdjacentIter<I: Iterator> {
    iter: Peekable<I>,
}

impl<I: Iterator<Item = (MemoryRange, T)>, T: PartialEq> Iterator for MergeAdjacentIter<I> {
    type Item = (MemoryRange, T);

    fn next(&mut self) -> Option<Self::Item> {
        let (first, typ) = self.iter.next()?;
        let mut start = first.start();
        let mut end = first.end();
        while let Some((r, _t)) = self.iter.next_if(|(r, t)| {
            assert!(r.start() >= start, "ranges are not sorted");
            assert!(r.start() >= end, "ranges overlap");
            r.start() == end && &typ == t
        }) {
            start = r.start();
            end = end.max(r.end());
        }
        Some((MemoryRange::new(first.start()..end), typ))
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::MemoryRange;
    use super::TWO_MB;
    use crate::AlignedSubranges;
    use crate::flatten_ranges;
    use crate::merge_adjacent_ranges;
    use crate::overlapping_ranges;
    use crate::subtract_ranges;
    use alloc::vec;
    use alloc::vec::Vec;

    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    #[test]
    fn test_align() {
        #[derive(Clone, Debug, PartialEq, Copy)]
        struct AlignedRangeResult {
            range: MemoryRange,
            page_size: u64,
        }

        let compare_with_base = |r1: Vec<MemoryRange>, base: u64, er: Vec<AlignedRangeResult>| {
            let result: Vec<_> = r1
                .iter()
                .flat_map(|range| AlignedSubranges::new(*range).with_offset(base))
                .collect();
            assert_eq!(result.len(), er.len());
            for (pos, range) in result.iter().enumerate() {
                assert_eq!(*range, er[pos].range);
                assert_eq!(range.alignment(base), er[pos].page_size);
            }
        };

        let compare = |r1: Vec<MemoryRange>, er: Vec<AlignedRangeResult>| {
            compare_with_base(r1, 0, er);
        };

        /// Builds a memory range with short hand.
        fn b_mr(start: u64, end: u64) -> MemoryRange {
            MemoryRange::new(start..end)
        }

        /// Builds a aligned range result with short hand.
        fn b_arr(range: MemoryRange, page_size: u64) -> AlignedRangeResult {
            AlignedRangeResult { range, page_size }
        }

        // [0, 4KB]
        let ram: Vec<_> = vec![b_mr(0x0, 4 * KB)];
        let expected_res: Vec<_> = vec![b_arr(ram[0], 1 << 12)];

        compare(ram, expected_res);

        // [0, 2MB]
        let ram: Vec<_> = vec![b_mr(0x0, 2 * MB)];
        let expected_res: Vec<_> = vec![b_arr(ram[0], 1 << 21)];
        compare(ram, expected_res);

        // [0, 1MB]
        let ram: Vec<_> = vec![b_mr(0x0, GB)];
        let expected_res: Vec<_> = vec![b_arr(ram[0], 1 << 30)];
        compare(ram, expected_res);

        // [6MB, 12.004MB]
        let ram: Vec<_> = vec![b_mr(6 * MB, 12 * MB + 4 * KB)];
        let expected_res: Vec<_> = vec![
            b_arr(b_mr(6 * MB, 12 * MB), TWO_MB),
            b_arr(b_mr(12 * MB, 12 * MB + 4 * KB), 1 << 12),
        ];
        compare(ram, expected_res);

        // [5.4MB, 12.2MB]
        let ram: Vec<_> = vec![b_mr(5 * MB + 400 * KB, 12 * MB + 400 * KB)];
        let expected_res: Vec<_> = vec![
            b_arr(b_mr(5 * MB + 400 * KB, 6 * MB), 1 << 14),
            b_arr(b_mr(6 * MB, 12 * MB), TWO_MB),
            b_arr(b_mr(12 * MB, 12 * MB + 400 * KB), 1 << 14),
        ];
        compare(ram, expected_res);

        // [1.501GB, 3.503GB]
        let ram: Vec<_> = vec![b_mr(GB + 501 * MB, 3 * GB + 503 * MB)];
        let expected_res: Vec<_> = vec![
            b_arr(b_mr(GB + 501 * MB, GB + 502 * MB), 1 << 20),
            b_arr(b_mr(GB + 502 * MB, 2 * GB), 1 << 21),
            b_arr(b_mr(2 * GB, 3 * GB), 1 << 30),
            b_arr(b_mr(3 * GB, 3 * GB + 502 * MB), 1 << 21),
            b_arr(b_mr(3 * GB + 502 * MB, 3 * GB + 503 * MB), 1 << 20),
        ];
        compare(ram, expected_res);

        // [4.008MB, 6.008MB] with necessary base to align up to 2MB
        let ram: Vec<_> = vec![b_mr(4 * MB + 8 * KB, 6 * MB + 8 * KB)];
        let base = 2 * MB - 8 * KB;
        let expected_res: Vec<_> = vec![b_arr(ram[0], TWO_MB)];
        compare_with_base(ram, base, expected_res);

        // [4.008MB, 6.008MB] without any base
        let ram: Vec<_> = vec![b_mr(4 * MB + 8 * KB, 6 * MB + 8 * KB)];
        let expected_res: Vec<_> = vec![b_arr(ram[0], 1 << 13)];
        compare_with_base(ram, 0, expected_res);
    }

    #[test]
    fn test_overlapping_ranges() {
        let left = [
            MemoryRange::new(0..4 * MB),
            MemoryRange::new(8 * MB..12 * MB),
            MemoryRange::new(12 * MB..12 * MB),
            MemoryRange::new(16 * MB..20 * MB),
            MemoryRange::new(24 * MB..32 * MB),
            MemoryRange::new(40 * MB..48 * MB),
        ];
        let right = [
            MemoryRange::new(2 * MB..6 * MB),
            MemoryRange::new(10 * MB..11 * MB),
            MemoryRange::new(11 * MB..11 * MB),
            MemoryRange::new(11 * MB..13 * MB),
            MemoryRange::new(15 * MB..22 * MB),
            MemoryRange::new(26 * MB..30 * MB),
        ];

        let result: Vec<_> = overlapping_ranges(left, right).collect();
        assert_eq!(
            result.as_slice(),
            &[
                MemoryRange::new(2 * MB..4 * MB),
                MemoryRange::new(10 * MB..11 * MB),
                MemoryRange::new(11 * MB..12 * MB),
                MemoryRange::new(16 * MB..20 * MB),
                MemoryRange::new(26 * MB..30 * MB),
            ]
        );
    }

    #[test]
    fn test_subtract_ranges() {
        let left = [
            MemoryRange::new(0..4 * MB),
            MemoryRange::new(8 * MB..12 * MB),
            MemoryRange::new(12 * MB..12 * MB),
            MemoryRange::new(16 * MB..20 * MB),
            MemoryRange::new(24 * MB..32 * MB),
            MemoryRange::new(40 * MB..48 * MB),
        ];
        let right = [
            MemoryRange::new(2 * MB..6 * MB),
            MemoryRange::new(10 * MB..11 * MB),
            MemoryRange::new(11 * MB..11 * MB),
            MemoryRange::new(11 * MB..13 * MB),
            MemoryRange::new(15 * MB..22 * MB),
            MemoryRange::new(26 * MB..30 * MB),
        ];

        let result: Vec<_> = subtract_ranges(left, right).collect();
        assert_eq!(
            result.as_slice(),
            &[
                MemoryRange::new(0..2 * MB),
                MemoryRange::new(8 * MB..10 * MB),
                MemoryRange::new(24 * MB..26 * MB),
                MemoryRange::new(30 * MB..32 * MB),
                MemoryRange::new(40 * MB..48 * MB),
            ]
        );
    }

    #[test]
    #[should_panic(expected = "left not sorted")]
    fn test_panic_unsorted_overlapping_left() {
        overlapping_ranges(
            [MemoryRange::new(MB..2 * MB), MemoryRange::new(0..MB)],
            [MemoryRange::new(3 * MB..4 * MB)],
        )
        .for_each(|_| ());
    }

    #[test]
    #[should_panic(expected = "right not sorted")]
    fn test_panic_unsorted_overlapping_right() {
        overlapping_ranges(
            [
                MemoryRange::new(MB..2 * MB),
                MemoryRange::new(3 * MB..4 * MB),
            ],
            [MemoryRange::new(0..MB), MemoryRange::new(0..MB)],
        )
        .for_each(|_| ());
    }

    #[test]
    #[should_panic(expected = "left not sorted")]
    fn test_panic_unsorted_subtract_left() {
        subtract_ranges(
            [MemoryRange::new(MB..2 * MB), MemoryRange::new(0..MB)],
            [MemoryRange::new(MB..2 * MB)],
        )
        .for_each(|_| ());
    }

    #[test]
    #[should_panic(expected = "right not sorted")]
    fn test_panic_unsorted_subtract_right() {
        subtract_ranges(
            [
                MemoryRange::new(MB..2 * MB),
                MemoryRange::new(3 * MB..4 * MB),
            ],
            [MemoryRange::new(MB..2 * MB), MemoryRange::new(MB..2 * MB)],
        )
        .for_each(|_| ());
    }

    #[test]
    fn test_aligned_subrange() {
        let test_cases = &[
            (0..0, MB, 0..0),
            (0..MB, MB, 0..MB),
            (4 * KB..MB + 4 * KB, MB, MB..MB),
            (MB..5 * MB, 2 * MB, 2 * MB..4 * MB),
        ];
        for (range, alignment, expected_aligned_range) in test_cases.iter().cloned() {
            assert_eq!(
                MemoryRange::new(range).aligned_subrange(alignment),
                MemoryRange::new(expected_aligned_range)
            );
        }
    }

    #[test]
    fn test_flatten_ranges() {
        let ranges =
            [0..4, 5..7, 6..11, 13..20, 20..25, 22..24, 35..36].map(MemoryRange::from_4k_gpn_range);
        let result = [0..4, 5..11, 13..25, 35..36].map(MemoryRange::from_4k_gpn_range);
        assert!(flatten_ranges(ranges).eq(result));
    }

    #[test]
    #[should_panic(expected = "ranges are not sorted")]
    fn test_flatten_ranges_not_sorted() {
        flatten_ranges([0..4, 5..7, 3..8].map(MemoryRange::from_4k_gpn_range)).for_each(|_| ());
    }

    #[test]
    fn test_merge_adjacent_ranges() {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Color {
            Red,
            Blue,
        }

        let ranges = [0..4, 5..7, 7..11, 11..12, 13..20, 20..25, 35..36]
            .map(MemoryRange::from_4k_gpn_range)
            .into_iter()
            .zip([
                Color::Red,
                Color::Red,
                Color::Red,
                Color::Blue,
                Color::Red,
                Color::Red,
                Color::Blue,
            ]);
        let result = [0..4, 5..11, 11..12, 13..25, 35..36]
            .map(MemoryRange::from_4k_gpn_range)
            .into_iter()
            .zip([Color::Red, Color::Red, Color::Blue, Color::Red, Color::Blue]);
        assert!(merge_adjacent_ranges(ranges).eq(result));
    }

    #[test]
    #[should_panic(expected = "ranges are not sorted")]
    fn test_merge_adjacent_ranges_not_sorted() {
        merge_adjacent_ranges([0..4, 5..7, 3..8].map(|r| (MemoryRange::from_4k_gpn_range(r), ())))
            .for_each(|_| ());
    }

    #[test]
    #[should_panic(expected = "ranges overlap")]
    fn test_merge_adjacent_ranges_overlap() {
        merge_adjacent_ranges([0..6, 5..7, 9..12].map(|r| (MemoryRange::from_4k_gpn_range(r), ())))
            .for_each(|_| ());
    }
}
