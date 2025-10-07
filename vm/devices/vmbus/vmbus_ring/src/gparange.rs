// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guestmem::ranges::PagedRange;
use smallvec::SmallVec;
use smallvec::smallvec;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const PAGE_SIZE: usize = 4096;

pub type GpnList = SmallVec<[u64; 64]>;

pub fn zeroed_gpn_list(len: usize) -> GpnList {
    smallvec![FromZeros::new_zeroed(); len]
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct GpaRange {
    pub len: u32,
    pub offset: u32,
}

#[derive(Debug, Default, Clone)]
pub struct MultiPagedRangeBuf<T: AsRef<[u64]>> {
    buf: T,
    count: usize,
}

impl<T: AsRef<[u64]>> MultiPagedRangeBuf<T> {
    pub fn validate(count: usize, buf: &[u64]) -> Result<(), Error> {
        let mut rem: &[u64] = buf;
        for _ in 0..count {
            let (_, rest) = parse(rem)?;
            rem = rest;
        }
        Ok(())
    }

    pub fn new(count: usize, buf: T) -> Result<Self, Error> {
        Self::validate(count, buf.as_ref())?;
        Ok(MultiPagedRangeBuf { buf, count })
    }

    pub fn subrange(
        &self,
        offset: usize,
        len: usize,
    ) -> Result<MultiPagedRangeBuf<GpnList>, Error> {
        if len == 0 {
            return Ok(MultiPagedRangeBuf::<GpnList>::empty());
        }

        let mut sub_buf = GpnList::new();
        let mut remaining_offset = offset;
        let mut remaining_length = len;
        let mut range_count = 0;
        for range in self.iter() {
            if let Some(n) = remaining_offset.checked_sub(range.len()) {
                remaining_offset = n;
                continue;
            }
            let cur_offset = std::mem::take(&mut remaining_offset);
            // Determine how many bytes we can take from this range after applying cur_offset.
            let available_here = range.len() - cur_offset;
            let take_len = available_here.min(remaining_length);
            let sub_range = range.subrange(cur_offset, take_len);

            sub_buf.push(u64::from_le_bytes(
                GpaRange {
                    len: sub_range.len() as u32,
                    offset: sub_range.offset() as u32,
                }
                .as_bytes()
                .try_into()
                .unwrap(),
            ));
            sub_buf.extend_from_slice(sub_range.gpns());
            range_count += 1;
            remaining_length -= sub_range.len();
            if remaining_length == 0 {
                break;
            }
        }

        if remaining_length > 0 {
            Err(Error::RangeTooSmall)
        } else {
            MultiPagedRangeBuf::<GpnList>::new(range_count, sub_buf)
        }
    }

    pub fn empty() -> Self
    where
        T: Default,
    {
        Self {
            buf: Default::default(),
            count: 0,
        }
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
        self.buf.as_ref()
    }

    pub fn into_buffer(self) -> T {
        self.buf
    }
}

impl MultiPagedRangeBuf<&'static [u64]> {
    pub const fn empty_const() -> Self {
        Self { buf: &[], count: 0 }
    }
}

impl<'a, T: AsRef<[u64]> + Default> IntoIterator for &'a MultiPagedRangeBuf<T> {
    type Item = PagedRange<'a>;
    type IntoIter = MultiPagedRangeIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> FromIterator<PagedRange<'a>> for MultiPagedRangeBuf<GpnList> {
    fn from_iter<I: IntoIterator<Item = PagedRange<'a>>>(iter: I) -> MultiPagedRangeBuf<GpnList> {
        let mut page_count = 0;
        let buf: GpnList = iter
            .into_iter()
            .map(|range| {
                let mut buf: GpnList = smallvec![u64::from_le_bytes(
                    GpaRange {
                        len: range.len() as u32,
                        offset: range.offset() as u32,
                    }
                    .as_bytes()
                    .try_into()
                    .unwrap()
                )];
                buf.extend_from_slice(range.gpns());
                page_count += 1;
                buf
            })
            .collect::<Vec<GpnList>>()
            .into_iter()
            .flatten()
            .collect();
        MultiPagedRangeBuf::<GpnList>::new(page_count, buf).unwrap()
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
    use guestmem::ranges::PagedRange;

    #[test]
    fn large_offset() {
        // Encode a header with offset having bits above the 12-bit page offset (0x1000)
        let hdr = GpaRange {
            len: 1,
            offset: 0x1000,
        };
        let mut buf: GpnList = GpnList::new();
        buf.push(u64::from_le_bytes(hdr.as_bytes().try_into().unwrap()));
        buf.push(0xdead_beef);

        // validate() should not accept the buffer
        let err = MultiPagedRangeBuf::new(1, buf).unwrap_err();
        assert!(matches!(err, Error::OffsetTooLarge));
    }

    // subrange should error when the requested span exceeds available bytes after offset.
    #[test]
    fn subrange_errors_when_span_beyond_total() {
        // Build a single-range buffer with 200 bytes starting at offset 100 within its first page.
        let gpns = [0x1000_u64];
        let range = PagedRange::new(100, 200, &gpns).expect("valid paged range");
        let ranges: MultiPagedRangeBuf<GpnList> = std::iter::once(range).collect();

        // Request a subrange starting 50 bytes into the buffer, of length 200 bytes.
        // Only 150 bytes remain (200 - 50), so this should be an error.
        let err = ranges.subrange(50, 200).unwrap_err();
        assert!(matches!(err, Error::RangeTooSmall));
    }

    // subrange across multiple ranges should split into partial
    // pieces with correct offsets, lengths, and page lists.
    #[test]
    fn subrange_spans_multiple_ranges() {
        let gpns1 = [1_u64, 2_u64];
        let gpns2 = [3_u64, 4_u64];
        // Two ranges: [100..400) over gpns1 and [0..500) over gpns2
        let r1 = PagedRange::new(100, 300, &gpns1).expect("r1");
        let r2 = PagedRange::new(0, 500, &gpns2).expect("r2");
        let ranges: MultiPagedRangeBuf<GpnList> = vec![r1, r2].into_iter().collect();

        // Take subrange starting 250 bytes into the concatenated ranges, length 200.
        // This yields 50 bytes from r1 (offset 350) and 150 bytes from r2 (offset 0).
        let sub = ranges.subrange(250, 200).expect("subrange ok");
        assert_eq!(sub.range_count(), 2);

        let mut it = sub.iter();
        let a = it.next().expect("first slice");
        assert_eq!(a.offset(), 350);
        assert_eq!(a.len(), 50);
        assert_eq!(a.gpns(), &gpns1[..1]);

        let b = it.next().expect("second slice");
        assert_eq!(b.offset(), 0);
        assert_eq!(b.len(), 150);
        assert_eq!(b.gpns(), &gpns2[..1]);

        assert!(it.next().is_none());
    }
}
