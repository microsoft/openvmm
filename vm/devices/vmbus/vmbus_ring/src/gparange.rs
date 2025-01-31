// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guestmem::ranges::PagedRange;
use smallvec::smallvec;
use smallvec::SmallVec;
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
            let cur_offset = if remaining_offset == 0 {
                0
            } else if remaining_offset > range.len() {
                remaining_offset -= range.len();
                continue;
            } else {
                let remaining = remaining_offset;
                remaining_offset = 0;
                remaining
            };

            let sub_range = match range.try_subrange(cur_offset, remaining_length) {
                Some(sub_range) => sub_range,
                None => range,
            };

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
    #[error("integer overflow")]
    Overflow,
}

fn parse(buf: &[u64]) -> Result<(PagedRange<'_>, &[u64]), Error> {
    let (hdr, gpas) = buf.split_first().ok_or(Error::EmptyRange)?;
    let byte_count = *hdr as u32;
    if byte_count == 0 {
        return Err(Error::EmptyByteCount);
    }
    let byte_offset = (*hdr >> 32) as u32 & 0xfff;
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
