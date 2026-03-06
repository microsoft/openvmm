// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types representing contiguous and discontiguous ranges of guest memory.
//!
//! The primary type is [`PagedRange`], which describes a single logically
//! contiguous byte range scattered across potentially-discontiguous guest
//! pages. It is used throughout the storage and virtio stacks to describe
//! IO buffers.
//!
//! # Memory model
//!
//! A `PagedRange` is defined by a GPN (guest page number) list, a starting
//! byte offset, and a length. The byte range is a contiguous window into the
//! flat byte space implied by concatenating the pages:
//!
//! ```text
//!   gpns:   [ GPN_0 ]  [ GPN_1 ]  [ GPN_2 ]  [ GPN_3 ]
//!   bytes:  0    4095  4096  8191  8192 12287 12288 16383
//!
//!                start                            end
//!                  │                               │
//!   GPN_0:  ░░░░░░░█████████   ◄── first page: partial (offset > 0)
//!   GPN_1:  ██████████████████ ◄── interior page: always fully covered
//!   GPN_2:  ██████████████████ ◄── interior page: always fully covered
//!   GPN_3:  █████████░░░░░░░░░ ◄── last page: partial (end < page boundary)
//! ```
//!
//! **Key constraint:** only the first and last pages may be partially
//! covered. All interior pages are implicitly fully used. There is no way
//! to express a gap or a non-zero starting offset on an interior page.
//!
//! This means you **cannot** combine two arbitrary guest memory regions
//! (e.g., two virtio descriptors with arbitrary GPAs) into a single
//! `PagedRange` unless every region boundary falls on a page boundary.
//! See [`PagedRanges`] for a type that can represent multiple disjoint
//! `PagedRange`s as a single logical buffer.

#![warn(missing_docs)]

use super::AccessError;
use super::GuestMemory;
use super::MemoryRead;
use super::MemoryWrite;
use super::PAGE_SIZE;
use super::PAGE_SIZE64;
use crate::InvalidGpn;

/// A range of bytes in the guest address space.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct AddressRange {
    /// The inclusive starting byte offset.
    pub start: u64,
    /// The exclusive ending byte offset.
    pub end: u64,
}

impl AddressRange {
    /// Returns the length of the range in bytes.
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Returns whether the range is empty.
    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

impl From<std::ops::Range<u64>> for AddressRange {
    fn from(range: std::ops::Range<u64>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

/// A single logically-contiguous byte range spread across guest pages.
///
/// `PagedRange` is defined by a GPN (guest page number) slice, a byte
/// offset into that slice's implied flat address space, and a length.
///
/// # Interior-page constraint
///
/// The first page may start at an arbitrary byte offset and the last page
/// may end before the page boundary, but **all interior pages are fully
/// covered**. There is no way to skip bytes within an interior page.
///
/// ```text
///   VALID:     ░░████  ██████  ██████  ███░░░
///   INVALID:   ░░████  ░░████  ██████  ███░░░
///                       ▲ interior page cannot start at non-zero offset
/// ```
///
/// This means two guest memory regions starting at arbitrary GPAs cannot
/// always be represented by a single `PagedRange`. Use [`PagedRanges`]
/// when you need to concatenate multiple independent regions.
///
/// # Construction
///
/// Use [`PagedRange::new(offset, len, gpns)`](PagedRange::new) where
/// `offset` is the byte offset into the page list and `len` is the total
/// byte length. The GPN slice must be large enough to cover
/// `offset + len` bytes.
#[derive(Debug, Copy, Clone)]
pub struct PagedRange<'a> {
    /// The starting offset in bytes from the beginning of the range described
    /// by `gpns`.
    start: usize,
    /// The ending offset in bytes from the beginning of the range described by
    /// `gpns`.
    end: usize,
    /// The page list describing the range that this is a subset of.
    gpns: &'a [u64],
}

impl<'a> PagedRange<'a> {
    /// The page size for GPNs. This is always 4KB.
    pub const PAGE_SIZE: usize = PAGE_SIZE;

    /// Creates a new range over `gpns`, starting at `offset` bytes into the page list, extending for `len` bytes.
    ///
    /// Returns `None` if `offset` or `len` are out of bounds.
    pub const fn new(offset: usize, len: usize, gpns: &'a [u64]) -> Option<Self> {
        let maxlen = gpns.len() * PAGE_SIZE;
        if maxlen < offset || maxlen - offset < len {
            return None;
        }
        Some(PagedRange {
            start: offset,
            end: offset + len,
            gpns,
        })
    }

    /// Returns the empty range.
    pub const fn empty() -> Self {
        PagedRange {
            start: 0,
            end: 0,
            gpns: &[],
        }
    }

    /// Returns a subrange of this range, or `None` if the subrange is outside this range.
    pub fn try_subrange(&self, offset: usize, len: usize) -> Option<Self> {
        if self.len() >= offset && self.len() - offset >= len {
            Some(PagedRange {
                start: self.start + offset,
                end: self.start + offset + len,
                gpns: self.gpns,
            })
        } else {
            None
        }
    }

    /// Returns a subrange of this range.
    ///
    /// Panics if the subrange is outside this range.
    #[track_caller]
    pub fn subrange(&self, offset: usize, len: usize) -> Self {
        self.try_subrange(offset, len)
            .unwrap_or_else(|| panic!("invalid subrange: {} + {} > {}", offset, len, self.len()))
    }

    /// Returns the length of the range in bytes.
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Returns whether the range is empty.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Returns the byte offset into the first page of the range.
    ///
    /// Only the first page can have a non-zero offset; all subsequent
    /// pages covered by the range are implicitly page-aligned.
    pub fn offset(&self) -> usize {
        self.start % PAGE_SIZE
    }

    /// Returns the range's list of page numbers.
    pub fn gpns(&self) -> &'a [u64] {
        let start_page = self.start / PAGE_SIZE;
        let end_page = self.end.div_ceil(PAGE_SIZE);
        &self.gpns[start_page..end_page]
    }

    /// Skips the first `len` bytes of the range.
    ///
    /// Panics if `len` is larger than the range's length.
    pub fn skip(&mut self, len: usize) {
        assert!(self.len() >= len);
        self.start += len;
    }

    /// Truncates the range to `len` bytes.
    ///
    /// Panics if `len` is larger than the range's length.
    pub fn truncate(&mut self, len: usize) {
        assert!(self.len() >= len);
        self.end = self.start + len;
    }

    /// Splits the range at `offset`.
    ///
    /// Panics if `offset` is outside the range.
    pub fn split(self, offset: usize) -> (Self, Self) {
        assert!(self.len() >= offset);
        (
            Self {
                start: self.start,
                end: self.start + offset,
                gpns: self.gpns,
            },
            Self {
                start: self.start + offset,
                end: self.end,
                gpns: self.gpns,
            },
        )
    }

    /// Splits the range at `offset`, returning `None` if `offset` is outside
    /// the range.
    pub fn try_split(self, offset: usize) -> Option<(Self, Self)> {
        if self.len() >= offset {
            Some((
                Self {
                    start: self.start,
                    end: self.start + offset,
                    gpns: self.gpns,
                },
                Self {
                    start: self.start + offset,
                    end: self.end,
                    gpns: self.gpns,
                },
            ))
        } else {
            None
        }
    }

    /// Removes and returns the first contiguous range.
    pub fn pop_front_range(&mut self) -> Option<Result<AddressRange, InvalidGpn>> {
        if self.is_empty() {
            None
        } else {
            let start_page = self.start / PAGE_SIZE;
            let end_page = self.end.div_ceil(PAGE_SIZE);
            let mut page = start_page + 1;
            while page < end_page && self.gpns[page - 1] + 1 == self.gpns[page] {
                page += 1;
            }

            let end = (page * PAGE_SIZE).min(self.end);

            let gpa = match crate::gpn_to_gpa(self.gpns[start_page]) {
                Ok(gpa) => gpa,
                Err(e) => return Some(Err(e)),
            };
            let start_gpa = gpa + self.start as u64 % PAGE_SIZE64;
            let range = AddressRange {
                start: start_gpa,
                end: start_gpa + (end - self.start) as u64,
            };
            self.start = end;
            Some(Ok(range))
        }
    }

    /// Returns a [`MemoryRead`] implementation.
    pub fn reader(self, mem: &'a GuestMemory) -> PagedRangeReader<'a> {
        PagedRangeReader { range: self, mem }
    }

    /// Returns a [`MemoryWrite`] implementation.
    pub fn writer(self, mem: &'a GuestMemory) -> PagedRangeWriter<'a> {
        PagedRangeWriter { range: self, mem }
    }

    /// Returns an iterator over the [`AddressRange`]s represented by this
    /// range.
    pub fn ranges(self) -> PagedRangeRangeIter<'a> {
        PagedRangeRangeIter(self)
    }
}

/// An iterator returned by [`PagedRange::ranges()`].
#[derive(Debug, Clone)]
pub struct PagedRangeRangeIter<'a>(PagedRange<'a>);

impl Iterator for PagedRangeRangeIter<'_> {
    type Item = Result<AddressRange, InvalidGpn>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.pop_front_range()
    }
}

/// A [`MemoryRead`] implementation for [`PagedRange`].
pub struct PagedRangeReader<'a> {
    range: PagedRange<'a>,
    mem: &'a GuestMemory,
}

impl MemoryRead for PagedRangeReader<'_> {
    fn read(&mut self, data: &mut [u8]) -> Result<&mut Self, AccessError> {
        let range = self
            .range
            .try_subrange(0, data.len())
            .ok_or_else(|| AccessError::OutOfRange(self.len(), data.len()))?;
        self.mem
            .read_range(&range, data)
            .map_err(AccessError::Memory)?;
        self.range.skip(data.len());
        Ok(self)
    }

    fn skip(&mut self, len: usize) -> Result<&mut Self, AccessError> {
        if self.len() < len {
            return Err(AccessError::OutOfRange(self.len(), len));
        }
        self.range.skip(len);
        Ok(self)
    }

    fn len(&self) -> usize {
        self.range.len()
    }
}

/// A [`MemoryWrite`] implementation for [`PagedRange`].
pub struct PagedRangeWriter<'a> {
    range: PagedRange<'a>,
    mem: &'a GuestMemory,
}

impl MemoryWrite for PagedRangeWriter<'_> {
    fn write(&mut self, data: &[u8]) -> Result<(), AccessError> {
        let range = self
            .range
            .try_subrange(0, data.len())
            .ok_or_else(|| AccessError::OutOfRange(self.len(), data.len()))?;
        self.mem
            .write_range(&range, data)
            .map_err(AccessError::Memory)?;
        self.range.skip(data.len());
        Ok(())
    }

    fn fill(&mut self, val: u8, len: usize) -> Result<(), AccessError> {
        let range = self
            .range
            .try_subrange(0, len)
            .ok_or_else(|| AccessError::OutOfRange(self.len(), len))?;
        self.mem
            .fill_range(&range, val)
            .map_err(AccessError::Memory)?;
        self.range.skip(len);
        Ok(())
    }

    fn len(&self) -> usize {
        self.range.len()
    }
}

/// A list of [`PagedRange`]s.
#[derive(Debug, Clone)]
pub struct PagedRanges<'a, T> {
    ranges: T,
    current: Option<PagedRange<'a>>,
    start: usize,
    end: usize,
}

impl<'a, T: Iterator<Item = PagedRange<'a>> + Clone> PagedRanges<'a, T> {
    /// Constructs a list wrapping an iterator.
    pub fn new<I>(ranges: I) -> Self
    where
        I: IntoIterator<IntoIter = T>,
    {
        let ranges = ranges.into_iter();
        let len = ranges.clone().map(|range| range.len()).sum();
        Self {
            ranges,
            current: None,
            start: 0,
            end: len,
        }
    }
}

impl<'a, T: Iterator<Item = PagedRange<'a>>> PagedRanges<'a, T> {
    /// Returns the total length in bytes of the ranges.
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Returns true if the range list is empty.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Advances the range list by `len` bytes.
    pub fn skip(&mut self, mut len: usize) {
        assert!(self.len() >= len);
        while len > 0 {
            let n = self.current(len).len();
            self.advance(n);
            len -= n;
        }
    }

    /// Returns a new list limited to `len` bytes.
    ///
    /// Panics if `len` is larger than the range's length.
    pub fn truncate(&mut self, len: usize) {
        assert!(self.len() >= len);
        self.end = self.start + len;
    }

    /// Returns an iterator of the remaining paged ranges.
    pub fn paged_ranges(self) -> PagedRangesIter<'a, T> {
        PagedRangesIter(self)
    }

    /// Returns a [`MemoryRead`] implementation for the ranges.
    pub fn reader(self, mem: &'a GuestMemory) -> PagedRangesReader<'a, T> {
        PagedRangesReader { views: self, mem }
    }

    /// Returns a [`MemoryWrite`] implementation for the ranges.
    pub fn writer(self, mem: &'a GuestMemory) -> PagedRangesWriter<'a, T> {
        PagedRangesWriter { views: self, mem }
    }

    fn current(&mut self, max_len: usize) -> PagedRange<'a> {
        debug_assert!(max_len <= self.len());
        if self.current.is_none() {
            self.current = self.ranges.next();
        }
        let range = self.current.unwrap();
        range.subrange(self.start, max_len.min(range.len() - self.start))
    }

    fn advance(&mut self, n: usize) {
        let current = self.current.as_ref().unwrap();
        self.start += n;
        if self.start == current.len() {
            self.current = None;
            self.end -= self.start;
            self.start = 0;
        }
    }
}

/// An iterator returned by [`PagedRanges::paged_ranges`].
pub struct PagedRangesIter<'a, T>(PagedRanges<'a, T>);

impl<'a, T: Iterator<Item = PagedRange<'a>>> Iterator for PagedRangesIter<'a, T> {
    type Item = PagedRange<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        let current = self
            .0
            .current
            .take()
            .or_else(|| self.0.ranges.next())
            .unwrap();
        let offset = self.0.start;
        let len = self.0.end.min(current.len()) - offset;
        self.0.end -= offset + len;
        self.0.current = None;
        self.0.start = 0;
        Some(current.subrange(offset, len))
    }
}

/// A [`MemoryRead`] implementation for [`PagedRanges`].
#[derive(Debug, Clone)]
pub struct PagedRangesReader<'a, T> {
    views: PagedRanges<'a, T>,
    mem: &'a GuestMemory,
}

impl<'a, T> PagedRangesReader<'a, T> {
    /// Returns the inner ranges.
    pub fn into_inner(self) -> PagedRanges<'a, T> {
        self.views
    }
}

impl<'a, T: Iterator<Item = PagedRange<'a>>> MemoryRead for PagedRangesReader<'a, T> {
    fn read(&mut self, mut data: &mut [u8]) -> Result<&mut Self, AccessError> {
        if self.len() < data.len() {
            return Err(AccessError::OutOfRange(self.len(), data.len()));
        }
        while !data.is_empty() {
            let range = self.views.current(data.len());
            let (buf, rest) = data.split_at_mut(range.len());
            self.mem
                .read_range(&range, buf)
                .map_err(AccessError::Memory)?;
            self.views.advance(range.len());
            data = rest;
        }
        Ok(self)
    }

    fn skip(&mut self, len: usize) -> Result<&mut Self, AccessError> {
        if self.len() < len {
            return Err(AccessError::OutOfRange(self.len(), len));
        }
        self.views.skip(len);
        Ok(self)
    }

    fn len(&self) -> usize {
        self.views.len()
    }
}

/// A [`MemoryWrite`] implementation for [`PagedRanges`].
#[derive(Debug)]
pub struct PagedRangesWriter<'a, T> {
    views: PagedRanges<'a, T>,
    mem: &'a GuestMemory,
}

impl<'a, T> PagedRangesWriter<'a, T> {
    /// Returns the inner ranges.
    pub fn into_inner(self) -> PagedRanges<'a, T> {
        self.views
    }
}

impl<'a, T: Iterator<Item = PagedRange<'a>>> MemoryWrite for PagedRangesWriter<'a, T> {
    fn write(&mut self, mut data: &[u8]) -> Result<(), AccessError> {
        if self.len() < data.len() {
            return Err(AccessError::OutOfRange(self.len(), data.len()));
        }
        while !data.is_empty() {
            let range = self.views.current(data.len());
            let (buf, rest) = data.split_at(range.len());
            self.mem
                .write_range(&range, buf)
                .map_err(AccessError::Memory)?;
            self.views.advance(range.len());
            data = rest;
        }
        Ok(())
    }

    fn fill(&mut self, val: u8, mut len: usize) -> Result<(), AccessError> {
        if self.len() < len {
            return Err(AccessError::OutOfRange(self.len(), len));
        }
        while len > 0 {
            let range = self.views.current(len);
            self.mem
                .fill_range(&range, val)
                .map_err(AccessError::Memory)?;
            self.views.advance(range.len());
            len -= range.len();
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.views.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ranges() {
        assert!(PagedRange::new(1, PAGE_SIZE, &[1]).is_none());

        let view1 = PagedRange::new(0x123, 0x2012, &[0x11, 0x22, 0x33]).unwrap();
        let view2 = PagedRange::new(0x456, 0x2017, &[0x111, 0x112, 0x222]).unwrap();
        let views = [view1, view2];
        let mut multi = PagedRanges::new(views.iter().copied());
        multi.skip(0x100);
        multi.truncate(0x2f29);

        let r1: Result<Vec<_>, _> = multi.paged_ranges().flat_map(|r| r.ranges()).collect();
        let r2: Vec<_> = [
            0x11223..0x12000,
            0x22000..0x23000,
            0x33000..0x33135,
            0x111456..0x11246d,
        ]
        .map(AddressRange::from)
        .to_vec();

        assert_eq!(&r1.unwrap(), &r2);
    }
}
