// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `no_std`-compatible primitives shared by [`guestmem`] and by embedded
//! guest-side crates that need to describe or access guest memory.
//!
//! [`guestmem`]: https://microsoft.github.io/openvmm/api/guestmem

#![no_std]
#![expect(missing_docs)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Debug;
use core::ops::Range;
use core::sync::atomic::AtomicU8;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub mod ranges;

/// Effective page size for page-related operations shared with `guestmem`.
pub const PAGE_SIZE: usize = 4096;
pub(crate) const PAGE_SIZE64: u64 = 4096;

/// A memory access error returned by one of the [`GuestMemory`] methods.
///
/// [`GuestMemory`]: https://microsoft.github.io/openvmm/api/guestmem/struct.GuestMemory.html
#[derive(Debug, Error)]
#[error(transparent)]
pub struct GuestMemoryError(Box<GuestMemoryErrorInner>);

impl GuestMemoryError {
    #[doc(hidden)]
    pub fn new(
        debug_name: &Arc<str>,
        range: Option<Range<u64>>,
        op: GuestMemoryOperation,
        err: GuestMemoryBackingError,
    ) -> Self {
        GuestMemoryError(Box::new(GuestMemoryErrorInner {
            op,
            debug_name: debug_name.clone(),
            range,
            gpa: (err.gpa != INVALID_ERROR_GPA).then_some(err.gpa),
            kind: err.kind,
            err: err.err,
        }))
    }

    /// Returns the kind of the error.
    pub fn kind(&self) -> GuestMemoryErrorKind {
        self.0.kind
    }
}

/// Describes the guest-memory operation that failed.
#[derive(Debug, Copy, Clone)]
pub enum GuestMemoryOperation {
    Read,
    Write,
    Fill,
    CompareExchange,
    Lock,
    Subrange,
    Probe,
}

impl core::fmt::Display for GuestMemoryOperation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.pad(match self {
            GuestMemoryOperation::Read => "read",
            GuestMemoryOperation::Write => "write",
            GuestMemoryOperation::Fill => "fill",
            GuestMemoryOperation::CompareExchange => "compare exchange",
            GuestMemoryOperation::Lock => "lock",
            GuestMemoryOperation::Subrange => "subrange",
            GuestMemoryOperation::Probe => "probe",
        })
    }
}

#[derive(Debug, Error)]
struct GuestMemoryErrorInner {
    op: GuestMemoryOperation,
    debug_name: Arc<str>,
    range: Option<Range<u64>>,
    gpa: Option<u64>,
    kind: GuestMemoryErrorKind,
    #[source]
    err: Box<dyn core::error::Error + Send + Sync>,
}

impl core::fmt::Display for GuestMemoryErrorInner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "guest memory '{debug_name}': {op} error: failed to access ",
            debug_name = self.debug_name,
            op = self.op
        )?;
        if let Some(range) = &self.range {
            write!(f, "{:#x}-{:#x}", range.start, range.end)?;
        } else {
            f.write_str("memory")?;
        }
        // Include the precise GPA if provided and different from the start of
        // the range.
        if let Some(gpa) = self.gpa {
            if self.range.as_ref().is_none_or(|range| range.start != gpa) {
                write!(f, " at {:#x}", gpa)?;
            }
        }
        Ok(())
    }
}

/// A memory access error returned by a [`GuestMemoryAccess`] trait method.
///
/// [`GuestMemoryAccess`]: https://microsoft.github.io/openvmm/api/guestmem/trait.GuestMemoryAccess.html
#[derive(Debug)]
pub struct GuestMemoryBackingError {
    #[doc(hidden)]
    pub gpa: u64,
    #[doc(hidden)]
    pub kind: GuestMemoryErrorKind,
    #[doc(hidden)]
    pub err: Box<dyn core::error::Error + Send + Sync>,
}

/// The kind of memory access error.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GuestMemoryErrorKind {
    /// An error that does not fit any other category.
    Other,
    /// The address is outside the valid range of the memory.
    OutOfRange,
    /// The memory has been protected by a higher virtual trust level.
    VtlProtected,
    /// The memory is shared but was accessed via a private address.
    NotPrivate,
    /// The memory is private but was accessed via a shared address.
    NotShared,
}

/// An error returned by a page fault handler in `GuestMemoryAccess::page_fault`.
pub struct PageFaultError {
    #[doc(hidden)]
    pub kind: GuestMemoryErrorKind,
    #[doc(hidden)]
    pub err: Box<dyn core::error::Error + Send + Sync>,
}

impl PageFaultError {
    /// Returns a new page fault error.
    pub fn new(
        kind: GuestMemoryErrorKind,
        err: impl Into<Box<dyn core::error::Error + Send + Sync>>,
    ) -> Self {
        Self {
            kind,
            err: err.into(),
        }
    }

    /// Returns a page fault error without an explicit kind.
    pub fn other(err: impl Into<Box<dyn core::error::Error + Send + Sync>>) -> Self {
        Self::new(GuestMemoryErrorKind::Other, err)
    }
}

/// Used to avoid needing an `Option` for [`GuestMemoryBackingError::gpa`], to
/// save size in hot paths.
pub(crate) const INVALID_ERROR_GPA: u64 = !0;

impl GuestMemoryBackingError {
    /// Returns a new error for a memory access failure at address `gpa`.
    pub fn new(
        kind: GuestMemoryErrorKind,
        gpa: u64,
        err: impl Into<Box<dyn core::error::Error + Send + Sync>>,
    ) -> Self {
        // `gpa` might incorrectly be INVALID_ERROR_GPA; this is harmless (just
        // affecting the error message), so don't assert on it in case this is
        // an untrusted value in some path.
        Self {
            kind,
            gpa,
            err: err.into(),
        }
    }

    /// Returns a new error without an explicit kind.
    pub fn other(gpa: u64, err: impl Into<Box<dyn core::error::Error + Send + Sync>>) -> Self {
        Self::new(GuestMemoryErrorKind::Other, gpa, err)
    }

    #[doc(hidden)]
    pub fn gpn(err: InvalidGpn) -> Self {
        Self {
            kind: GuestMemoryErrorKind::OutOfRange,
            gpa: INVALID_ERROR_GPA,
            err: err.into(),
        }
    }
}

#[derive(Debug, Error)]
#[error("invalid guest page number {0:#x}")]
pub struct InvalidGpn(u64);

pub fn gpn_to_gpa(gpn: u64) -> Result<u64, InvalidGpn> {
    gpn.checked_mul(PAGE_SIZE64).ok_or(InvalidGpn(gpn))
}

/// The `[AtomicU8; PAGE_SIZE]` representation of a single page.
pub type Page = [AtomicU8; PAGE_SIZE];

/// An error accessing byte-oriented guest memory via [`MemoryRead`] or
/// [`MemoryWrite`].
#[derive(Debug, Error)]
pub enum AccessError {
    #[error("memory access error")]
    Memory(#[from] GuestMemoryError),
    #[error("out of range: {0:#x} < {1:#x}")]
    OutOfRange(usize, usize),
    #[error("write attempted to read-only memory")]
    ReadOnly,
}

pub trait MemoryRead {
    fn read(&mut self, data: &mut [u8]) -> Result<&mut Self, AccessError>;
    fn skip(&mut self, len: usize) -> Result<&mut Self, AccessError>;
    fn len(&self) -> usize;

    fn read_plain<T: IntoBytes + FromBytes + Immutable + KnownLayout>(
        &mut self,
    ) -> Result<T, AccessError> {
        let mut value: T = FromZeros::new_zeroed();
        self.read(value.as_mut_bytes())?;
        Ok(value)
    }

    fn read_n<T: IntoBytes + FromBytes + Immutable + KnownLayout + Copy>(
        &mut self,
        len: usize,
    ) -> Result<Vec<T>, AccessError> {
        let mut value = vec![FromZeros::new_zeroed(); len];
        self.read(value.as_mut_bytes())?;
        Ok(value)
    }

    fn read_all(&mut self) -> Result<Vec<u8>, AccessError> {
        let mut value = vec![0; self.len()];
        self.read(&mut value)?;
        Ok(value)
    }

    fn limit(self, len: usize) -> Limit<Self>
    where
        Self: Sized,
    {
        let len = len.min(self.len());
        Limit { inner: self, len }
    }
}

/// A trait for sequentially updating a region of memory.
pub trait MemoryWrite {
    fn write(&mut self, data: &[u8]) -> Result<(), AccessError>;
    fn zero(&mut self, len: usize) -> Result<(), AccessError> {
        self.fill(0, len)
    }
    fn fill(&mut self, val: u8, len: usize) -> Result<(), AccessError>;

    /// The space remaining in the memory region.
    fn len(&self) -> usize;

    fn limit(self, len: usize) -> Limit<Self>
    where
        Self: Sized,
    {
        let len = len.min(self.len());
        Limit { inner: self, len }
    }
}

impl MemoryRead for &'_ [u8] {
    fn read(&mut self, data: &mut [u8]) -> Result<&mut Self, AccessError> {
        if self.len() < data.len() {
            return Err(AccessError::OutOfRange(self.len(), data.len()));
        }
        let (source, rest) = self.split_at(data.len());
        data.copy_from_slice(source);
        *self = rest;
        Ok(self)
    }

    fn skip(&mut self, len: usize) -> Result<&mut Self, AccessError> {
        if self.len() < len {
            return Err(AccessError::OutOfRange(self.len(), len));
        }
        *self = &self[len..];
        Ok(self)
    }

    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

impl MemoryWrite for &mut [u8] {
    fn write(&mut self, data: &[u8]) -> Result<(), AccessError> {
        if self.len() < data.len() {
            return Err(AccessError::OutOfRange(self.len(), data.len()));
        }
        let (dest, rest) = core::mem::take(self).split_at_mut(data.len());
        dest.copy_from_slice(data);
        *self = rest;
        Ok(())
    }

    fn fill(&mut self, val: u8, len: usize) -> Result<(), AccessError> {
        if self.len() < len {
            return Err(AccessError::OutOfRange(self.len(), len));
        }
        let (dest, rest) = core::mem::take(self).split_at_mut(len);
        dest.fill(val);
        *self = rest;
        Ok(())
    }

    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

/// Wraps a [`MemoryRead`] / [`MemoryWrite`] with a byte cap.
#[derive(Debug, Clone)]
pub struct Limit<T> {
    inner: T,
    len: usize,
}

impl<T: MemoryRead> MemoryRead for Limit<T> {
    fn read(&mut self, data: &mut [u8]) -> Result<&mut Self, AccessError> {
        let len = data.len();
        if len > self.len {
            return Err(AccessError::OutOfRange(self.len, len));
        }
        self.inner.read(data)?;
        self.len -= len;
        Ok(self)
    }

    fn skip(&mut self, len: usize) -> Result<&mut Self, AccessError> {
        if len > self.len {
            return Err(AccessError::OutOfRange(self.len, len));
        }
        self.inner.skip(len)?;
        self.len -= len;
        Ok(self)
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl<T: MemoryWrite> MemoryWrite for Limit<T> {
    fn write(&mut self, data: &[u8]) -> Result<(), AccessError> {
        let len = data.len();
        if len > self.len {
            return Err(AccessError::OutOfRange(self.len, len));
        }
        self.inner.write(data)?;
        self.len -= len;
        Ok(())
    }

    fn fill(&mut self, val: u8, len: usize) -> Result<(), AccessError> {
        if len > self.len {
            return Err(AccessError::OutOfRange(self.len, len));
        }
        self.inner.fill(val, len)?;
        self.len -= len;
        Ok(())
    }

    fn len(&self) -> usize {
        self.len
    }
}
