// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! [`GuestMemory`]-backed readers and writers for guest memory ranges.
//!
//! The core range descriptors — [`PagedRange`] and [`PagedRanges`] — live in
//! [`guestmem_core::ranges`] so they can be used from `no_std` contexts. This
//! module supplies the `std`-only bridge between those descriptors and
//! [`GuestMemory`].

pub use guestmem_core::ranges::PagedRange;
pub use guestmem_core::ranges::PagedRanges;

use super::AccessError;
use super::GuestMemory;
use super::MemoryRead;
use super::MemoryWrite;

/// Adapter trait attaching a [`GuestMemory`] backing to a range descriptor to
/// produce byte-oriented readers and writers.
///
/// This trait is the extension point that lets [`PagedRange`] and
/// [`PagedRanges`] — which by themselves are pure address-space descriptors
/// with no attached memory — be read from or written to. It is implemented for
/// both range types in this module and is not intended to be implemented by
/// downstream crates.
///
/// The typical call pattern is:
///
/// ```ignore
/// use guestmem::MemoryRead;
/// use guestmem::ranges::GuestMemoryView;
///
/// let mut reader = range.reader(&mem);
/// let value: MyStruct = reader.read_plain()?;
/// ```
pub trait GuestMemoryView<'a> {
    /// The [`MemoryRead`] implementation type
    type Reader: MemoryRead;

    /// The [`MemoryWrite`] implementation type
    type Writer: MemoryWrite;

    /// Returns a [`MemoryRead`] implementation for the ranges.
    fn reader(self, mem: &'a GuestMemory) -> Self::Reader;

    /// Returns a [`MemoryWrite`] implementation for the ranges.
    fn writer(self, mem: &'a GuestMemory) -> Self::Writer;
}

impl<'a, T: Iterator<Item = PagedRange<'a>>> GuestMemoryView<'a> for PagedRanges<'a, T> {
    type Reader = PagedRangesReader<'a, T>;
    type Writer = PagedRangesWriter<'a, T>;

    fn reader(self, mem: &'a GuestMemory) -> Self::Reader {
        PagedRangesReader { views: self, mem }
    }

    fn writer(self, mem: &'a GuestMemory) -> Self::Writer {
        PagedRangesWriter { views: self, mem }
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

impl<'a> GuestMemoryView<'a> for PagedRange<'a> {
    type Reader = PagedRangeReader<'a>;

    type Writer = PagedRangeWriter<'a>;

    fn reader(self, mem: &'a GuestMemory) -> PagedRangeReader<'a> {
        PagedRangeReader { range: self, mem }
    }

    fn writer(self, mem: &'a GuestMemory) -> PagedRangeWriter<'a> {
        PagedRangeWriter { range: self, mem }
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
