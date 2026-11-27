// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use alloc::boxed::Box;
use alloc::vec::Vec;
use guestmem_core::ranges::PagedRange;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const PAGE_SIZE: usize = 4096;

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GpaRange {
    pub len: u32,
    pub offset: u32,
}

/// Validates that `buf` contains `count` valid GPA ranges. Returns the number
/// of `u64` entries actually used to describe those ranges.
pub fn validate_gpa_ranges(count: usize, buf: &[u64]) -> Result<usize, Error> {
    let mut rem: &[u64] = buf;
    for _ in 0..count {
        let (_, rest) = parse(rem)?;
        rem = rest;
    }
    Ok(buf.len() - rem.len())
}

#[derive(Debug, Default, Clone)]
pub struct MultiPagedRangeBuf {
    /// The buffer used to store the range data, concatenated. Each range
    /// consists of a [`GpaRange`] header followed by the list of GPNs.
    /// Note that `size_of::<GpaRange>() == size_of::<u64>()`.
    buf: Box<[u64]>,
    /// The number of u64 elements in the buffer that are valid. Data after this
    /// point is initialized from Rust's point of view but is not logically part of
    /// the ranges.
    valid: usize,
    /// The number of ranges stored in the buffer.
    count: usize,
}

impl MultiPagedRangeBuf {
    pub fn from_range_buffer(count: usize, mut buf: Vec<u64>) -> Result<Self, Error> {
        let valid = validate_gpa_ranges(count, buf.as_ref())?;
        buf.truncate(valid);
        Ok(MultiPagedRangeBuf {
            buf: buf.into_boxed_slice(),
            valid,
            count,
        })
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn iter(&self) -> MultiPagedRangeIter<'_> {
        MultiPagedRangeIter {
            buf: self.buf.as_ref(),
            count: self.count,
        }
    }

    pub fn range_count(&self) -> usize {
        self.count
    }

    pub fn first(&self) -> Option<PagedRange<'_>> {
        self.iter().next()
    }

    /// Validates that this multi range consists of exactly one range that is
    /// page aligned. Returns that range.
    pub fn contiguous_aligned(&self) -> Option<PagedRange<'_>> {
        if self.count != 1 {
            return None;
        }
        let first = self.first()?;
        if first.offset() != 0 || first.len() % PAGE_SIZE != 0 {
            return None;
        }
        Some(first)
    }

    pub fn range_buffer(&self) -> &[u64] {
        &self.buf[..self.valid]
    }

    /// Clears the buffer and resets the range count to zero.
    pub fn clear(&mut self) {
        self.valid = 0;
        self.count = 0;
    }

    fn ensure_space(&mut self, additional: usize) -> &mut [u64] {
        let required = self.valid + additional;
        if required > self.buf.len() {
            self.resize_buffer(required);
        }
        &mut self.buf[self.valid..required]
    }

    #[cold]
    fn resize_buffer(&mut self, new_size: usize) {
        // Use `Vec`'s resizing logic to get appropriate growth behavior, but
        // initialize all the data to make updating it easier.
        let mut buf: Vec<u64> = core::mem::take(&mut self.buf).into();
        buf.resize(new_size, 0);
        // Initialize the rest of the capacity that `Vec` allocated.
        buf.resize(buf.capacity(), 0);
        self.buf = buf.into_boxed_slice();
    }

    /// Appends a new paged range to the buffer.
    pub fn push_range(&mut self, range: PagedRange<'_>) {
        let len = 1 + range.gpns().len();
        let buf = self.ensure_space(len);
        let hdr = GpaRange {
            len: range.len() as u32,
            offset: range.offset() as u32,
        };
        buf[0] = zerocopy::transmute!(hdr);
        buf[1..].copy_from_slice(range.gpns());
        self.count += 1;
        self.valid += len;
    }

    /// Attempts to extend the buffer by `count` ranges, requiring `len` u64
    /// entries in total. `f` is called to fill in the newly allocated
    /// buffer space.
    ///
    /// If `f` returns an error, the buffer is restored to its
    /// previous state and the error is propagated. If `f` returns `Ok(())`,
    /// the newly added ranges are validated; if validation fails, the buffer
    /// is restored and the validation error is returned inside `Ok(Err(_))`.
    pub fn try_extend_with<E>(
        &mut self,
        len: usize,
        count: usize,
        f: impl FnOnce(&mut [u64]) -> Result<(), E>,
    ) -> Result<Result<(), Error>, E> {
        let buf = self.ensure_space(len);
        f(buf)?;
        let valid_len = match validate_gpa_ranges(count, buf) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        // Now that validation succeeded, update the buffer state. Failure
        // before this may have expanded the buffer but did not affect the
        // visible behavior of this object.
        self.valid += valid_len;
        self.count += count;
        Ok(Ok(()))
    }
}

impl<'a> IntoIterator for &'a MultiPagedRangeBuf {
    type Item = PagedRange<'a>;
    type IntoIter = MultiPagedRangeIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> FromIterator<PagedRange<'a>> for MultiPagedRangeBuf {
    fn from_iter<I: IntoIterator<Item = PagedRange<'a>>>(iter: I) -> MultiPagedRangeBuf {
        let mut this = MultiPagedRangeBuf::new();
        for range in iter {
            this.push_range(range);
        }
        this
    }
}

#[derive(Clone, Debug)]
pub struct MultiPagedRangeIter<'a> {
    buf: &'a [u64],
    count: usize,
}

impl<'a> Iterator for MultiPagedRangeIter<'a> {
    type Item = PagedRange<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.count == 0 {
            return None;
        }
        let hdr = GpaRange::read_from_prefix(self.buf[0].as_bytes())
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let page_count = ((hdr.offset + hdr.len) as usize).div_ceil(PAGE_SIZE); // N.B. already validated
        let (this, rest) = self.buf.split_at(page_count + 1);
        let range = PagedRange::new(hdr.offset as usize, hdr.len as usize, &this[1..]).unwrap();
        self.count -= 1;
        self.buf = rest;
        Some(range)
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("empty range")]
    EmptyRange,
    #[error("empty byte count")]
    EmptyByteCount,
    #[error("range too small")]
    RangeTooSmall,
    #[error("byte offset too large")]
    OffsetTooLarge,
    #[error("integer overflow")]
    Overflow,
}

fn parse(buf: &[u64]) -> Result<(PagedRange<'_>, &[u64]), Error> {
    let (hdr, gpas) = buf.split_first().ok_or(Error::EmptyRange)?;
    let byte_count = *hdr as u32;
    if byte_count == 0 {
        return Err(Error::EmptyByteCount);
    }
    let byte_offset = (*hdr >> 32) as u32;
    if byte_offset > 0xfff {
        return Err(Error::OffsetTooLarge);
    }
    let pages = (byte_count
        .checked_add(4095)
        .ok_or(Error::Overflow)?
        .checked_add(byte_offset)
        .ok_or(Error::Overflow)?) as usize
        / PAGE_SIZE;
    if gpas.len() < pages {
        return Err(Error::RangeTooSmall);
    }
    let (gpas, rest) = gpas.split_at(pages);
    assert!(!gpas.is_empty());
    Ok((
        PagedRange::new(byte_offset as usize, byte_count as usize, gpas)
            .expect("already validated"),
        rest,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn large_offset() {
        // Encode a header with offset having bits above the 12-bit page offset (0x1000)
        let hdr = GpaRange {
            len: 1,
            offset: 0x1000,
        };
        let buf = vec![
            u64::from_le_bytes(hdr.as_bytes().try_into().unwrap()),
            0xdead_beef,
        ];

        // validate() should not accept the buffer
        let err = MultiPagedRangeBuf::from_range_buffer(1, buf).unwrap_err();
        assert!(matches!(err, Error::OffsetTooLarge));
    }

    fn ranges(buffer: &MultiPagedRangeBuf) -> Vec<(usize, usize, Vec<u64>)> {
        buffer
            .into_iter()
            .map(|range| (range.offset(), range.len(), range.gpns().to_vec()))
            .collect()
    }

    // Test: Parse concatenated ranges with an unaligned page-spanning range.
    // Expected: Offsets, lengths, and page numbers are preserved; trailing data is ignored.
    #[test]
    fn parses_multiple_ranges_and_trims_trailing_data() {
        let encoded = vec![8, 10, (4090u64 << 32) | 16, 20, 30, u64::MAX];
        assert_eq!(validate_gpa_ranges(2, &encoded).unwrap(), 5);
        let buffer = MultiPagedRangeBuf::from_range_buffer(2, encoded.clone()).unwrap();
        assert_eq!(buffer.range_count(), 2);
        assert_eq!(buffer.range_buffer(), &encoded[..5]);
        assert_eq!(
            ranges(&buffer),
            [(0, 8, vec![10]), (4090, 16, vec![20, 30])]
        );
        assert!(buffer.contiguous_aligned().is_none());
    }

    // Test: Parse truncated, empty, overflowing, and oversized-offset GPA ranges.
    // Expected: Each malformed encoding returns the corresponding validation error.
    #[test]
    fn malformed_ranges_return_validation_errors() {
        for (encoded, expected) in [
            (vec![], Error::EmptyRange),
            (vec![0], Error::EmptyByteCount),
            (vec![4097, 1], Error::RangeTooSmall),
            (vec![(4096u64 << 32) | 1, 1], Error::OffsetTooLarge),
            (vec![u64::from(u32::MAX), 1], Error::Overflow),
            (
                vec![(4095u64 << 32) | u64::from(u32::MAX - 4095), 1],
                Error::Overflow,
            ),
        ] {
            let error = MultiPagedRangeBuf::from_range_buffer(1, encoded).unwrap_err();
            assert_eq!(
                core::mem::discriminant(&error),
                core::mem::discriminant(&expected),
                "expected {expected:?}, got {error:?}",
            );
        }
    }

    // Test: Append ranges past the initial allocation, clear, and reuse the buffer.
    // Expected: All appended ranges are returned in order and clearing removes all old data.
    #[test]
    fn appended_ranges_survive_growth_and_clear() {
        let mut buffer = MultiPagedRangeBuf::new();
        assert!(buffer.first().is_none());
        assert!(buffer.contiguous_aligned().is_none());
        for page in 0..64u64 {
            buffer.push_range(PagedRange::new(0, 4096, &[page]).unwrap());
        }
        assert_eq!(buffer.range_count(), 64);
        assert_eq!(
            ranges(&buffer),
            (0..64)
                .map(|page| (0, 4096, vec![page]))
                .collect::<Vec<_>>()
        );
        buffer.clear();
        assert_eq!(buffer.range_count(), 0);
        assert!(buffer.range_buffer().is_empty());
        assert!(buffer.first().is_none());
        buffer.push_range(PagedRange::new(0, 4096, &[99]).unwrap());
        assert_eq!(buffer.contiguous_aligned().unwrap().gpns(), [99]);
        assert_eq!(ranges(&buffer.clone()), [(0, 4096, vec![99])]);
    }

    // Test: Extend an existing buffer with a failing callback or malformed range.
    // Expected: Neither failure changes visible ranges; a later valid extension succeeds.
    #[test]
    fn failed_extensions_preserve_existing_ranges() {
        let mut buffer: MultiPagedRangeBuf = [PagedRange::new(0, 4096, &[7]).unwrap()]
            .into_iter()
            .collect();
        let expected = ranges(&buffer);
        assert_eq!(
            buffer
                .try_extend_with(100, 1, |space| {
                    space[..2].copy_from_slice(&[8, 9]);
                    Err("copy failed")
                })
                .unwrap_err(),
            "copy failed"
        );
        assert_eq!(ranges(&buffer), expected);
        assert_eq!(buffer.range_buffer(), [4096, 7]);
        let result = buffer.try_extend_with(2, 1, |space| {
            space.copy_from_slice(&[0, 8]);
            Ok::<_, ()>(())
        });
        assert!(matches!(result, Ok(Err(Error::EmptyByteCount))));
        assert_eq!(ranges(&buffer), expected);
        buffer
            .try_extend_with(3, 1, |space| {
                space.copy_from_slice(&[8, 9, u64::MAX]);
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(ranges(&buffer), [(0, 4096, vec![7]), (0, 8, vec![9])]);
        assert_eq!(buffer.range_buffer(), [4096, 7, 8, 9]);
    }
}
