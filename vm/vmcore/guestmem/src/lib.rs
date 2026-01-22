// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interfaces to read and write guest memory.

// UNSAFETY: This crate's whole purpose is manual memory mapping and management.
#![expect(unsafe_code)]
#![expect(missing_docs)]

pub mod ranges;

use self::ranges::PagedRange;
use inspect::Inspect;
use pal_event::Event;
use sparse_mmap::AsMappableRef;
use std::any::Any;
use std::fmt::Debug;
use std::io;
use std::ops::Deref;
use std::ops::DerefMut;
use std::ops::Range;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// Effective page size for page-related operations in this crate.
pub const PAGE_SIZE: usize = 4096;
const PAGE_SIZE64: u64 = 4096;

/// A memory access error returned by one of the [`GuestMemory`] methods.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct GuestMemoryError(Box<GuestMemoryErrorInner>);

impl GuestMemoryError {
    fn new(
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

#[derive(Debug, Copy, Clone)]
enum GuestMemoryOperation {
    Read,
    Write,
    Fill,
    CompareExchange,
    Lock,
    Subrange,
    Probe,
}

impl std::fmt::Display for GuestMemoryOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
    err: Box<dyn std::error::Error + Send + Sync>,
}

impl std::fmt::Display for GuestMemoryErrorInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
#[derive(Debug)]
pub struct GuestMemoryBackingError {
    gpa: u64,
    kind: GuestMemoryErrorKind,
    err: Box<dyn std::error::Error + Send + Sync>,
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

/// An error returned by a page fault handler in [`GuestMemoryAccess::page_fault`].
pub struct PageFaultError {
    kind: GuestMemoryErrorKind,
    err: Box<dyn std::error::Error + Send + Sync>,
}

impl PageFaultError {
    /// Returns a new page fault error.
    pub fn new(
        kind: GuestMemoryErrorKind,
        err: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self {
            kind,
            err: err.into(),
        }
    }

    /// Returns a page fault error without an explicit kind.
    pub fn other(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::new(GuestMemoryErrorKind::Other, err)
    }
}

/// Used to avoid needing an `Option` for [`GuestMemoryBackingError::gpa`], to
/// save size in hot paths.
const INVALID_ERROR_GPA: u64 = !0;

impl GuestMemoryBackingError {
    /// Returns a new error for a memory access failure at address `gpa`.
    pub fn new(
        kind: GuestMemoryErrorKind,
        gpa: u64,
        err: impl Into<Box<dyn std::error::Error + Send + Sync>>,
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
    pub fn other(gpa: u64, err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::new(GuestMemoryErrorKind::Other, gpa, err)
    }

    fn gpn(err: InvalidGpn) -> Self {
        Self {
            kind: GuestMemoryErrorKind::OutOfRange,
            gpa: INVALID_ERROR_GPA,
            err: err.into(),
        }
    }
}

#[derive(Debug, Error)]
#[error("no memory at address")]
struct OutOfRange;

#[derive(Debug, Error)]
#[error("memory not lockable")]
struct NotLockable;

#[derive(Debug, Error)]
#[error("no fallback for this operation")]
struct NoFallback;

#[derive(Debug, Error)]
#[error("the specified page is not mapped")]
struct NotMapped;

#[derive(Debug, Error)]
#[error("page inaccessible in bitmap")]
struct BitmapFailure;

/// A trait for a guest memory backing that is fully available via a virtual
/// address mapping, as opposed to the fallback functions such as
/// [`GuestMemoryAccess::read_fallback`].
///
/// By implementing this trait, a type guarantees that its
/// [`GuestMemoryAccess::mapping`] will return `Some(_)` and that all of its
/// memory can be accessed through that mapping, without needing to call the
/// fallback functions.
pub trait LinearGuestMemory: GuestMemoryAccess {}

// SAFETY: the allocation will stay valid for the lifetime of the object.
unsafe impl GuestMemoryAccess for sparse_mmap::alloc::SharedMem {
    fn mapping(&self) -> Option<NonNull<u8>> {
        NonNull::new(self.as_ptr().cast_mut().cast())
    }

    fn max_address(&self) -> u64 {
        self.len() as u64
    }
}

impl LinearGuestMemory for sparse_mmap::alloc::SharedMem {}

/// A page-aligned heap allocation for use with [`GuestMemory`].
pub struct AlignedHeapMemory {
    pages: Box<[AlignedPage]>,
}

impl Debug for AlignedHeapMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlignedHeapMemory")
            .field("len", &self.len())
            .finish()
    }
}

#[repr(C, align(4096))]
struct AlignedPage([AtomicU8; PAGE_SIZE]);

impl AlignedHeapMemory {
    /// Allocates a new memory of `size` bytes, rounded up to a page size.
    pub fn new(size: usize) -> Self {
        #[expect(clippy::declare_interior_mutable_const)] // <https://github.com/rust-lang/rust-clippy/issues/7665>
        const ZERO: AtomicU8 = AtomicU8::new(0);
        #[expect(clippy::declare_interior_mutable_const)]
        const ZERO_PAGE: AlignedPage = AlignedPage([ZERO; PAGE_SIZE]);
        let mut pages = Vec::new();
        pages.resize_with(size.div_ceil(PAGE_SIZE), || ZERO_PAGE);
        Self {
            pages: pages.into(),
        }
    }

    /// Returns the length of the memory in bytes.
    pub fn len(&self) -> usize {
        self.pages.len() * PAGE_SIZE
    }

    /// Returns an immutable slice of bytes.
    ///
    /// This must take `&mut self` since the buffer is mutable via interior
    /// mutability with just `&self`.
    pub fn as_bytes(&mut self) -> &[u8] {
        self.as_mut()
    }

    /// Returns a mutable slice of bytes.
    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        self.as_mut()
    }
}

impl Deref for AlignedHeapMemory {
    type Target = [AtomicU8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: the buffer has the correct size and validity.
        unsafe { std::slice::from_raw_parts(self.pages.as_ptr().cast(), self.len()) }
    }
}

impl DerefMut for AlignedHeapMemory {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the buffer is unaliased and valid.
        unsafe { std::slice::from_raw_parts_mut(self.pages.as_mut_ptr().cast(), self.len()) }
    }
}

impl AsRef<[AtomicU8]> for AlignedHeapMemory {
    fn as_ref(&self) -> &[AtomicU8] {
        self
    }
}

impl AsMut<[AtomicU8]> for AlignedHeapMemory {
    fn as_mut(&mut self) -> &mut [AtomicU8] {
        self
    }
}

impl AsMut<[u8]> for AlignedHeapMemory {
    fn as_mut(&mut self) -> &mut [u8] {
        // FUTURE: use AtomicU8::get_mut_slice once stabilized.
        // SAFETY: the buffer is unaliased, so it is fine to cast away the atomicness of the
        // slice.
        unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr().cast(), self.len()) }
    }
}

// SAFETY: the allocation remains alive and valid for the lifetime of the
// object.
unsafe impl GuestMemoryAccess for AlignedHeapMemory {
    fn mapping(&self) -> Option<NonNull<u8>> {
        NonNull::new(self.pages.as_ptr().cast_mut().cast())
    }

    fn max_address(&self) -> u64 {
        (self.pages.len() * PAGE_SIZE) as u64
    }
}

impl LinearGuestMemory for AlignedHeapMemory {}

/// A trait for a guest memory backing.
///
/// Guest memory may be backed by a virtual memory mapping, in which case this
/// trait can provide the VA and length of that mapping. Alternatively, it may
/// be backed by some other means, in which case this trait can provide fallback
/// methods for reading and writing memory.
///
/// Memory access should first be attempted via the virtual address mapping. If
/// this fails or is not present, the caller should fall back to `read_fallback`
/// or `write_fallback`. This allows an implementation to have a fast path using
/// the mapping, and a slow path using the fallback functions.
///
/// # Safety
///
/// The implementor must follow the contract for each method.
pub unsafe trait GuestMemoryAccess: 'static + Send + Sync {
    /// Returns a stable VA mapping for guest memory.
    ///
    /// The size of the mapping is the same as `max_address`.
    ///
    /// The VA is guaranteed to remain reserved, but individual ranges may be
    /// uncommitted.
    fn mapping(&self) -> Option<NonNull<u8>>;

    /// The maximum address that can be passed to the `*_fallback` methods, as
    /// well as the maximum offset into the VA range described by `mapping`.
    fn max_address(&self) -> u64;

    /// The bitmaps to check for validity, one bit per page. If a bit is set,
    /// then the page is valid to access via the mapping; if it is clear, then
    /// the page will not be accessed.
    ///
    /// The bitmaps must be at least `ceil(bitmap_start + max_address() /
    /// PAGE_SIZE)` bits long, and they must be valid for atomic read access for
    /// the lifetime of this object from any thread.
    ///
    /// The bitmaps are only checked if there is a mapping. If the bitmap check
    /// fails, then the associated `*_fallback` routine is called to handle the
    /// error.
    ///
    /// Bitmap checks are performed under the [`rcu()`] RCU domain, with relaxed
    /// accesses. After a thread updates the bitmap to be more restrictive, it
    /// must call [minircu::RcuDomain::synchronize()] on [`minircu::global()`]
    /// to ensure that all threads see the update before taking any action that
    /// depends on the bitmap update being visible.
    #[cfg(feature = "bitmap")]
    fn access_bitmap(&self) -> Option<BitmapInfo> {
        None
    }

    // Returns an accessor for a subrange, or `None` to use the default
    // implementation.
    fn subrange(
        &self,
        offset: u64,
        len: u64,
        allow_preemptive_locking: bool,
    ) -> Result<Option<GuestMemory>, GuestMemoryBackingError> {
        let _ = (offset, len, allow_preemptive_locking);
        Ok(None)
    }

    /// Called when access to memory via the mapped range fails, either due to a
    /// bitmap failure or due to a failure when accessing the virtual address.
    ///
    /// `address` is the address where the access failed. `len` is the remainder
    /// of the access; it is not necessarily the case that all `len` bytes are
    /// inaccessible in the bitmap or mapping.
    ///
    /// Returns whether the faulting operation should be retried, failed, or that
    /// one of the fallback operations (e.g. `read_fallback`) should be called.
    fn page_fault(
        &self,
        address: u64,
        len: usize,
        write: bool,
        bitmap_failure: bool,
    ) -> PageFaultAction {
        let _ = (address, len, write);
        let err = if bitmap_failure {
            PageFaultError::other(BitmapFailure)
        } else {
            PageFaultError::other(NotMapped)
        };
        PageFaultAction::Fail(err)
    }

    /// Fallback called if a read fails via direct access to `mapped_range`.
    ///
    /// This is only called if `mapping()` returns `None` or if `page_fault()`
    /// returns `PageFaultAction::Fallback`.
    ///
    /// Implementors must ensure that `dest[..len]` is fully initialized on
    /// successful return.
    ///
    /// # Safety
    /// The caller must ensure that `dest[..len]` is valid for write. Note,
    /// however, that `dest` might be aliased by other threads, the guest, or
    /// the kernel.
    unsafe fn read_fallback(
        &self,
        addr: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let _ = (dest, len);
        Err(GuestMemoryBackingError::other(addr, NoFallback))
    }

    /// Fallback called if a write fails via direct access to `mapped_range`.
    ///
    /// This is only called if `mapping()` returns `None` or if `page_fault()`
    /// returns `PageFaultAction::Fallback`.
    ///
    /// # Safety
    /// The caller must ensure that `src[..len]` is valid for read. Note,
    /// however, that `src` might be aliased by other threads, the guest, or
    /// the kernel.
    unsafe fn write_fallback(
        &self,
        addr: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let _ = (src, len);
        Err(GuestMemoryBackingError::other(addr, NoFallback))
    }

    /// Fallback called if a fill fails via direct access to `mapped_range`.
    ///
    /// This is only called if `mapping()` returns `None` or if `page_fault()`
    /// returns `PageFaultAction::Fallback`.
    fn fill_fallback(&self, addr: u64, val: u8, len: usize) -> Result<(), GuestMemoryBackingError> {
        let _ = (val, len);
        Err(GuestMemoryBackingError::other(addr, NoFallback))
    }

    /// Fallback called if a compare exchange fails via direct access to `mapped_range`.
    ///
    /// On compare failure, returns `Ok(false)` and updates `current`.
    ///
    /// This is only called if `mapping()` returns `None` or if `page_fault()`
    /// returns `PageFaultAction::Fallback`.
    fn compare_exchange_fallback(
        &self,
        addr: u64,
        current: &mut [u8],
        new: &[u8],
    ) -> Result<bool, GuestMemoryBackingError> {
        let _ = (current, new);
        Err(GuestMemoryBackingError::other(addr, NoFallback))
    }

    /// Prepares a guest page for having its virtual address exposed as part of
    /// a lock call.
    ///
    /// This is useful to ensure that the address is mapped in a way that it can
    /// be passed to the kernel for DMA.
    fn expose_va(&self, address: u64, len: u64) -> Result<(), GuestMemoryBackingError> {
        let _ = (address, len);
        Ok(())
    }

    /// Returns the base IO virtual address for the mapping.
    ///
    /// This is the base address that should be used for DMA from a user-mode
    /// device driver whose device is not otherwise configured to go through an
    /// IOMMU.
    fn base_iova(&self) -> Option<u64> {
        None
    }

    /// Locks the specified guest physical pages (GPNs), preventing any mapping
    /// or permission changes until they are unlocked.
    ///
    /// Returns a boolean indicating whether unlocking is required.
    fn lock_gpns(&self, gpns: &[u64]) -> Result<bool, GuestMemoryBackingError> {
        let _ = gpns;
        Ok(false)
    }

    /// Unlocks the specified guest physical pages (GPNs) after exclusive access.
    ///
    /// Panics if asked to unlock a page that was not previously locked. The
    /// caller must ensure that the given slice has the same ordering as the
    /// one passed to `lock_gpns`.
    fn unlock_gpns(&self, gpns: &[u64]) {
        let _ = gpns;
    }
}

trait DynGuestMemoryAccess: 'static + Send + Sync + Any {
    fn subrange(
        &self,
        offset: u64,
        len: u64,
        allow_preemptive_locking: bool,
    ) -> Result<Option<GuestMemory>, GuestMemoryBackingError>;

    fn page_fault(
        &self,
        address: u64,
        len: usize,
        write: bool,
        bitmap_failure: bool,
    ) -> PageFaultAction;

    /// # Safety
    /// See [`GuestMemoryAccess::read_fallback`].
    unsafe fn read_fallback(
        &self,
        addr: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError>;

    /// # Safety
    /// See [`GuestMemoryAccess::write_fallback`].
    unsafe fn write_fallback(
        &self,
        addr: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError>;

    fn fill_fallback(&self, addr: u64, val: u8, len: usize) -> Result<(), GuestMemoryBackingError>;

    fn compare_exchange_fallback(
        &self,
        addr: u64,
        current: &mut [u8],
        new: &[u8],
    ) -> Result<bool, GuestMemoryBackingError>;

    fn expose_va(&self, address: u64, len: u64) -> Result<(), GuestMemoryBackingError>;

    fn lock_gpns(&self, gpns: &[u64]) -> Result<bool, GuestMemoryBackingError>;

    fn unlock_gpns(&self, gpns: &[u64]);
}

impl<T: GuestMemoryAccess> DynGuestMemoryAccess for T {
    fn subrange(
        &self,
        offset: u64,
        len: u64,
        allow_preemptive_locking: bool,
    ) -> Result<Option<GuestMemory>, GuestMemoryBackingError> {
        self.subrange(offset, len, allow_preemptive_locking)
    }

    fn page_fault(
        &self,
        address: u64,
        len: usize,
        write: bool,
        bitmap_failure: bool,
    ) -> PageFaultAction {
        self.page_fault(address, len, write, bitmap_failure)
    }

    unsafe fn read_fallback(
        &self,
        addr: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        // SAFETY: guaranteed by caller.
        unsafe { self.read_fallback(addr, dest, len) }
    }

    unsafe fn write_fallback(
        &self,
        addr: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        // SAFETY: guaranteed by caller.
        unsafe { self.write_fallback(addr, src, len) }
    }

    fn fill_fallback(&self, addr: u64, val: u8, len: usize) -> Result<(), GuestMemoryBackingError> {
        self.fill_fallback(addr, val, len)
    }

    fn compare_exchange_fallback(
        &self,
        addr: u64,
        current: &mut [u8],
        new: &[u8],
    ) -> Result<bool, GuestMemoryBackingError> {
        self.compare_exchange_fallback(addr, current, new)
    }

    fn expose_va(&self, address: u64, len: u64) -> Result<(), GuestMemoryBackingError> {
        self.expose_va(address, len)
    }

    fn lock_gpns(&self, gpns: &[u64]) -> Result<bool, GuestMemoryBackingError> {
        self.lock_gpns(gpns)
    }

    fn unlock_gpns(&self, gpns: &[u64]) {
        self.unlock_gpns(gpns)
    }
}

/// The action to take after [`GuestMemoryAccess::page_fault`] returns to
/// continue the operation.
pub enum PageFaultAction {
    /// Fail the operation.
    Fail(PageFaultError),
    /// Retry the operation.
    Retry,
    /// Use the fallback method to access the memory.
    Fallback,
}

/// Returned by [`GuestMemoryAccess::access_bitmap`].
#[cfg(feature = "bitmap")]
pub struct BitmapInfo {
    /// A pointer to the bitmap for read access.
    pub read_bitmap: NonNull<u8>,
    /// A pointer to the bitmap for write access.
    pub write_bitmap: NonNull<u8>,
    /// The bit offset of the beginning of the bitmap.
    ///
    /// Typically this is zero, but it is needed to support subranges that are
    /// not 8-page multiples.
    pub bit_offset: u8,
}

// SAFETY: passing through guarantees from `T`.
unsafe impl<T: GuestMemoryAccess> GuestMemoryAccess for Arc<T> {
    fn mapping(&self) -> Option<NonNull<u8>> {
        self.as_ref().mapping()
    }

    fn max_address(&self) -> u64 {
        self.as_ref().max_address()
    }

    #[cfg(feature = "bitmap")]
    fn access_bitmap(&self) -> Option<BitmapInfo> {
        self.as_ref().access_bitmap()
    }

    fn subrange(
        &self,
        offset: u64,
        len: u64,
        allow_preemptive_locking: bool,
    ) -> Result<Option<GuestMemory>, GuestMemoryBackingError> {
        self.as_ref()
            .subrange(offset, len, allow_preemptive_locking)
    }

    fn page_fault(
        &self,
        addr: u64,
        len: usize,
        write: bool,
        bitmap_failure: bool,
    ) -> PageFaultAction {
        self.as_ref().page_fault(addr, len, write, bitmap_failure)
    }

    unsafe fn read_fallback(
        &self,
        addr: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        // SAFETY: passing through guarantees from caller.
        unsafe { self.as_ref().read_fallback(addr, dest, len) }
    }

    unsafe fn write_fallback(
        &self,
        addr: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        // SAFETY: passing through guarantees from caller.
        unsafe { self.as_ref().write_fallback(addr, src, len) }
    }

    fn fill_fallback(&self, addr: u64, val: u8, len: usize) -> Result<(), GuestMemoryBackingError> {
        self.as_ref().fill_fallback(addr, val, len)
    }

    fn compare_exchange_fallback(
        &self,
        addr: u64,
        current: &mut [u8],
        new: &[u8],
    ) -> Result<bool, GuestMemoryBackingError> {
        self.as_ref().compare_exchange_fallback(addr, current, new)
    }

    fn expose_va(&self, address: u64, len: u64) -> Result<(), GuestMemoryBackingError> {
        self.as_ref().expose_va(address, len)
    }

    fn base_iova(&self) -> Option<u64> {
        self.as_ref().base_iova()
    }
}

// SAFETY: the allocation will stay valid for the lifetime of the object.
unsafe impl GuestMemoryAccess for sparse_mmap::SparseMapping {
    fn mapping(&self) -> Option<NonNull<u8>> {
        NonNull::new(self.as_ptr().cast())
    }

    fn max_address(&self) -> u64 {
        self.len() as u64
    }
}

/// Default guest memory range type, enforcing access boundaries.
struct GuestMemoryAccessRange {
    base: Arc<GuestMemoryInner>,
    offset: u64,
    len: u64,
    region: usize,
}

impl GuestMemoryAccessRange {
    fn adjust_range(&self, address: u64, len: u64) -> Result<u64, GuestMemoryBackingError> {
        if address <= self.len && len <= self.len - address {
            Ok(self.offset + address)
        } else {
            Err(GuestMemoryBackingError::new(
                GuestMemoryErrorKind::OutOfRange,
                address,
                OutOfRange,
            ))
        }
    }
}

// SAFETY: `mapping()` is guaranteed to be valid for the lifetime of the object.
unsafe impl GuestMemoryAccess for GuestMemoryAccessRange {
    fn mapping(&self) -> Option<NonNull<u8>> {
        let region = &self.base.regions[self.region];
        region.mapping.and_then(|mapping| {
            let offset = self.offset & self.base.region_def.region_mask;
            // This is guaranteed by construction.
            assert!(region.len >= offset + self.len);
            // SAFETY: this mapping is guaranteed to be within range by
            // construction (and validated again via the assertion above).
            NonNull::new(unsafe { mapping.0.as_ptr().add(offset as usize) })
        })
    }

    fn max_address(&self) -> u64 {
        self.len
    }

    #[cfg(feature = "bitmap")]
    fn access_bitmap(&self) -> Option<BitmapInfo> {
        let region = &self.base.regions[self.region];
        region.bitmaps.map(|bitmaps| {
            let offset = self.offset & self.base.region_def.region_mask;
            let bit_offset = region.bitmap_start as u64 + offset / PAGE_SIZE64;
            let [read_bitmap, write_bitmap] = bitmaps.map(|SendPtrU8(ptr)| {
                // SAFETY: the bitmap is guaranteed to be big enough for the region
                // by construction.
                NonNull::new(unsafe { ptr.as_ptr().add((bit_offset / 8) as usize) }).unwrap()
            });
            let bitmap_start = (bit_offset % 8) as u8;
            BitmapInfo {
                read_bitmap,
                write_bitmap,
                bit_offset: bitmap_start,
            }
        })
    }

    fn subrange(
        &self,
        offset: u64,
        len: u64,
        _allow_preemptive_locking: bool,
    ) -> Result<Option<GuestMemory>, GuestMemoryBackingError> {
        let address = self.adjust_range(offset, len)?;
        Ok(Some(GuestMemory::new(
            self.base.debug_name.clone(),
            GuestMemoryAccessRange {
                base: self.base.clone(),
                offset: address,
                len,
                region: self.region,
            },
        )))
    }

    fn page_fault(
        &self,
        address: u64,
        len: usize,
        write: bool,
        bitmap_failure: bool,
    ) -> PageFaultAction {
        let address = self
            .adjust_range(address, len as u64)
            .expect("the caller should have validated the range was in the mapping");

        self.base
            .imp
            .page_fault(address, len, write, bitmap_failure)
    }

    unsafe fn write_fallback(
        &self,
        address: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let address = self.adjust_range(address, len as u64)?;
        // SAFETY: guaranteed by caller.
        unsafe { self.base.imp.write_fallback(address, src, len) }
    }

    fn fill_fallback(
        &self,
        address: u64,
        val: u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let address = self.adjust_range(address, len as u64)?;
        self.base.imp.fill_fallback(address, val, len)
    }

    fn compare_exchange_fallback(
        &self,
        addr: u64,
        current: &mut [u8],
        new: &[u8],
    ) -> Result<bool, GuestMemoryBackingError> {
        let address = self.adjust_range(addr, new.len() as u64)?;
        self.base
            .imp
            .compare_exchange_fallback(address, current, new)
    }

    unsafe fn read_fallback(
        &self,
        address: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let address = self.adjust_range(address, len as u64)?;
        // SAFETY: guaranteed by caller.
        unsafe { self.base.imp.read_fallback(address, dest, len) }
    }

    fn expose_va(&self, address: u64, len: u64) -> Result<(), GuestMemoryBackingError> {
        let address = self.adjust_range(address, len)?;
        self.base.imp.expose_va(address, len)
    }

    fn base_iova(&self) -> Option<u64> {
        let region = &self.base.regions[self.region];
        Some(region.base_iova? + (self.offset & self.base.region_def.region_mask))
    }
}

/// Create a default guest memory subrange that verifies range limits and calls
/// back into the base implementation.
fn create_memory_subrange(
    base: Arc<GuestMemoryInner>,
    offset: u64,
    len: u64,
    _allow_preemptive_locking: bool,
) -> Result<GuestMemory, GuestMemoryBackingError> {
    let (_, _, region) = base.region(offset, len)?;
    Ok(GuestMemory::new(
        base.debug_name.clone(),
        GuestMemoryAccessRange {
            base,
            offset,
            len,
            region,
        },
    ))
}

struct MultiRegionGuestMemoryAccess<T> {
    imps: Vec<Option<T>>,
    region_def: RegionDefinition,
}

impl<T> MultiRegionGuestMemoryAccess<T> {
    fn region(&self, gpa: u64, len: u64) -> Result<(&T, u64), GuestMemoryBackingError> {
        let (i, offset) = self.region_def.region(gpa, len)?;
        let imp = self.imps[i].as_ref().ok_or(GuestMemoryBackingError::new(
            GuestMemoryErrorKind::OutOfRange,
            gpa,
            OutOfRange,
        ))?;
        Ok((imp, offset))
    }
}

// SAFETY: `mapping()` is unreachable and panics if called.
impl<T: GuestMemoryAccess> DynGuestMemoryAccess for MultiRegionGuestMemoryAccess<T> {
    fn subrange(
        &self,
        offset: u64,
        len: u64,
        allow_preemptive_locking: bool,
    ) -> Result<Option<GuestMemory>, GuestMemoryBackingError> {
        let (region, offset_in_region) = self.region(offset, len)?;
        region.subrange(offset_in_region, len, allow_preemptive_locking)
    }

    unsafe fn read_fallback(
        &self,
        addr: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let (region, offset_in_region) = self.region(addr, len as u64)?;
        // SAFETY: guaranteed by caller.
        unsafe { region.read_fallback(offset_in_region, dest, len) }
    }

    unsafe fn write_fallback(
        &self,
        addr: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let (region, offset_in_region) = self.region(addr, len as u64)?;
        // SAFETY: guaranteed by caller.
        unsafe { region.write_fallback(offset_in_region, src, len) }
    }

    fn fill_fallback(&self, addr: u64, val: u8, len: usize) -> Result<(), GuestMemoryBackingError> {
        let (region, offset_in_region) = self.region(addr, len as u64)?;
        region.fill_fallback(offset_in_region, val, len)
    }

    fn compare_exchange_fallback(
        &self,
        addr: u64,
        current: &mut [u8],
        new: &[u8],
    ) -> Result<bool, GuestMemoryBackingError> {
        let (region, offset_in_region) = self.region(addr, new.len() as u64)?;
        region.compare_exchange_fallback(offset_in_region, current, new)
    }

    fn expose_va(&self, address: u64, len: u64) -> Result<(), GuestMemoryBackingError> {
        let (region, offset_in_region) = self.region(address, len)?;
        region.expose_va(offset_in_region, len)
    }

    fn page_fault(
        &self,
        address: u64,
        len: usize,
        write: bool,
        bitmap_failure: bool,
    ) -> PageFaultAction {
        match self.region(address, len as u64) {
            Ok((region, offset_in_region)) => {
                region.page_fault(offset_in_region, len, write, bitmap_failure)
            }
            Err(err) => PageFaultAction::Fail(PageFaultError {
                kind: err.kind,
                err: err.err,
            }),
        }
    }

    fn lock_gpns(&self, gpns: &[u64]) -> Result<bool, GuestMemoryBackingError> {
        let mut ret = false;
        for gpn in gpns {
            let (region, offset_in_region) = self.region(gpn * PAGE_SIZE64, PAGE_SIZE64)?;
            ret |= region.lock_gpns(&[offset_in_region / PAGE_SIZE64])?;
        }
        Ok(ret)
    }

    fn unlock_gpns(&self, gpns: &[u64]) {
        for gpn in gpns {
            let (region, offset_in_region) = self.region(gpn * PAGE_SIZE64, PAGE_SIZE64).unwrap();
            region.unlock_gpns(&[offset_in_region / PAGE_SIZE64]);
        }
    }
}

/// A wrapper around a `GuestMemoryAccess` that provides methods for safely
/// reading and writing guest memory.
// NOTE: this type uses `inspect(skip)`, as it end up being a dependency of
// _many_ objects, and littering the inspect graph with references to the same
// node would be silly.
#[derive(Debug, Clone, Inspect)]
#[inspect(skip)]
pub struct GuestMemory {
    inner: Arc<GuestMemoryInner>,
}

struct GuestMemoryInner<T: ?Sized = dyn DynGuestMemoryAccess> {
    region_def: RegionDefinition,
    regions: Vec<MemoryRegion>,
    debug_name: Arc<str>,
    allocated: bool,
    imp: T,
}

impl<T: ?Sized> Debug for GuestMemoryInner<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestMemoryInner")
            .field("region_def", &self.region_def)
            .field("regions", &self.regions)
            .finish()
    }
}

#[derive(Debug, Copy, Clone, Default)]
struct MemoryRegion {
    mapping: Option<SendPtrU8>,
    #[cfg(feature = "bitmap")]
    bitmaps: Option<[SendPtrU8; 2]>,
    #[cfg(feature = "bitmap")]
    bitmap_start: u8,
    len: u64,
    base_iova: Option<u64>,
}

/// The access type. The values correspond to bitmap indexes.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum AccessType {
    Read = 0,
    Write = 1,
}

/// `NonNull<u8>` that implements `Send+Sync`.
///
/// Rust makes pointers `!Send+!Sync` by default to force you to think about the
/// ownership model and thread safety of types using pointers--there is nothing
/// safety-related about `Send`/`Sync` on pointers by themselves since all such
/// accesses to pointers require `unsafe` blocks anyway.
///
/// However, in practice, this leads to spurious manual `Send+Sync` impls on
/// types containing pointers, especially those containing generics. Define a
/// wrapping pointer type that implements `Send+Sync` so that the normal auto
/// trait rules apply to types containing these pointers.
#[derive(Debug, Copy, Clone)]
struct SendPtrU8(NonNull<u8>);

// SAFETY: see type description.
unsafe impl Send for SendPtrU8 {}
// SAFETY: see type description.
unsafe impl Sync for SendPtrU8 {}

impl MemoryRegion {
    fn new(imp: &impl GuestMemoryAccess) -> Self {
        #[cfg(feature = "bitmap")]
        let (bitmaps, bitmap_start) = {
            let bitmap_info = imp.access_bitmap();
            let bitmaps = bitmap_info
                .as_ref()
                .map(|bm| [SendPtrU8(bm.read_bitmap), SendPtrU8(bm.write_bitmap)]);
            let bitmap_start = bitmap_info.map_or(0, |bi| bi.bit_offset);
            (bitmaps, bitmap_start)
        };
        Self {
            mapping: imp.mapping().map(SendPtrU8),
            #[cfg(feature = "bitmap")]
            bitmaps,
            #[cfg(feature = "bitmap")]
            bitmap_start,
            len: imp.max_address(),
            base_iova: imp.base_iova(),
        }
    }

    /// # Safety
    ///
    /// The caller must ensure that `offset + len` fits in this region, and that
    /// the object bitmap is currently valid for atomic read access from this
    /// thread.
    unsafe fn check_access(
        &self,
        access_type: AccessType,
        offset: u64,
        len: u64,
    ) -> Result<(), u64> {
        debug_assert!(self.len >= offset + len);
        #[cfg(not(feature = "bitmap"))]
        let _ = access_type;

        #[cfg(feature = "bitmap")]
        if let Some(bitmaps) = &self.bitmaps {
            let SendPtrU8(bitmap) = bitmaps[access_type as usize];
            let start = offset / PAGE_SIZE64;
            let end = (offset + len - 1) / PAGE_SIZE64;
            // FUTURE: consider optimizing this separately for multi-page and
            // single-page accesses.
            for gpn in start..=end {
                let bit_offset = self.bitmap_start as u64 + gpn;
                // SAFETY: the caller ensures that the bitmap is big enough and
                // valid for atomic read access from this thread.
                let bit = unsafe {
                    (*bitmap
                        .as_ptr()
                        .cast_const()
                        .cast::<AtomicU8>()
                        .add(bit_offset as usize / 8))
                    .load(std::sync::atomic::Ordering::Relaxed)
                        & (1 << (bit_offset % 8))
                };
                if bit == 0 {
                    return Err((gpn * PAGE_SIZE64).saturating_sub(offset));
                }
            }
        }
        Ok(())
    }
}

/// The default implementation is [`GuestMemory::empty`].
impl Default for GuestMemory {
    fn default() -> Self {
        Self::empty()
    }
}

struct Empty;

// SAFETY: the mapping is empty, so all requirements are trivially satisfied.
unsafe impl GuestMemoryAccess for Empty {
    fn mapping(&self) -> Option<NonNull<u8>> {
        None
    }

    fn max_address(&self) -> u64 {
        0
    }
}

#[derive(Debug, Error)]
pub enum MultiRegionError {
    #[error("region size {0:#x} is not a power of 2")]
    NotPowerOfTwo(u64),
    #[error("region size {0:#x} is smaller than a page")]
    RegionSizeTooSmall(u64),
    #[error(
        "too many regions ({region_count}) for region size {region_size:#x}; max is {max_region_count}"
    )]
    TooManyRegions {
        region_count: usize,
        max_region_count: usize,
        region_size: u64,
    },
    #[error("backing size {backing_size:#x} is too large for region size {region_size:#x}")]
    BackingTooLarge { backing_size: u64, region_size: u64 },
}

/// The RCU domain memory accesses occur under. Updates to any memory access
/// bitmaps must be synchronized under this domain.
///
/// See [`GuestMemoryAccess::access_bitmap`] for more details.
///
/// This is currently the global domain, but this is reexported here to make
/// calling code clearer.
#[cfg(feature = "bitmap")]
pub fn rcu() -> minircu::RcuDomain {
    // Use the global domain unless we find a reason to do something else.
    minircu::global()
}

impl GuestMemory {
    /// Returns a new instance using `imp` as the backing.
    ///
    /// `debug_name` is used to specify which guest memory is being accessed in
    /// error messages.
    pub fn new(debug_name: impl Into<Arc<str>>, imp: impl GuestMemoryAccess) -> Self {
        // Install signal handlers on unix if a mapping is present.
        //
        // Skip this on miri even when there is a mapping, since the mapping may
        // never be accessed by the code under test.
        if imp.mapping().is_some() && !cfg!(miri) {
            trycopy::initialize_try_copy();
        }
        Self::new_inner(debug_name.into(), imp, false)
    }

    fn new_inner(debug_name: Arc<str>, imp: impl GuestMemoryAccess, allocated: bool) -> Self {
        let regions = vec![MemoryRegion::new(&imp)];
        Self {
            inner: Arc::new(GuestMemoryInner {
                imp,
                debug_name,
                region_def: RegionDefinition {
                    invalid_mask: 1 << 63,
                    region_mask: !0 >> 1,
                    region_bits: 63, // right shift of 64 isn't valid, so restrict the space
                },
                regions,
                allocated,
            }),
        }
    }

    /// Creates a new multi-region guest memory, made up of multiple mappings.
    /// This allows you to create a very large sparse layout (up to the limits
    /// of the VM's physical address space) without having to allocate an
    /// enormous amount of virtual address space.
    ///
    /// Each region will be `region_size` bytes and will start immediately after
    /// the last one. This must be a power of two, be at least a page in size,
    /// and cannot fill the full 64-bit address space.
    ///
    /// `imps` must be a list of [`GuestMemoryAccess`] implementations, one for
    /// each region. Use `None` if the corresponding region is empty.
    ///
    /// A region's mapping cannot fully fill the region. This is necessary to
    /// avoid callers expecting to be able to access a memory range that spans
    /// two regions.
    pub fn new_multi_region(
        debug_name: impl Into<Arc<str>>,
        region_size: u64,
        mut imps: Vec<Option<impl GuestMemoryAccess>>,
    ) -> Result<Self, MultiRegionError> {
        // Install signal handlers.
        trycopy::initialize_try_copy();

        if !region_size.is_power_of_two() {
            return Err(MultiRegionError::NotPowerOfTwo(region_size));
        }
        if region_size < PAGE_SIZE64 {
            return Err(MultiRegionError::RegionSizeTooSmall(region_size));
        }
        let region_bits = region_size.trailing_zeros();

        let max_region_count = 1 << (63 - region_bits);

        let region_count = imps.len().next_power_of_two();
        if region_count > max_region_count {
            return Err(MultiRegionError::TooManyRegions {
                region_count,
                max_region_count,
                region_size,
            });
        }

        let valid_bits = region_bits + region_count.trailing_zeros();
        assert!(valid_bits < 64);
        let invalid_mask = !0 << valid_bits;

        let mut regions = vec![MemoryRegion::default(); region_count];
        for (imp, region) in imps.iter().zip(&mut regions) {
            let Some(imp) = imp else { continue };
            let backing_size = imp.max_address();
            if backing_size > region_size {
                return Err(MultiRegionError::BackingTooLarge {
                    backing_size,
                    region_size,
                });
            }
            *region = MemoryRegion::new(imp);
        }

        let region_def = RegionDefinition {
            invalid_mask,
            region_mask: region_size - 1,
            region_bits,
        };

        imps.resize_with(region_count, || None);
        let imp = MultiRegionGuestMemoryAccess { imps, region_def };

        let inner = GuestMemoryInner {
            debug_name: debug_name.into(),
            region_def,
            regions,
            imp,
            allocated: false,
        };

        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Allocates a guest memory object on the heap with the given size in
    /// bytes.
    ///
    /// `size` will be rounded up to the page size. The backing buffer will be
    /// page aligned.
    ///
    /// The debug name in errors will be "heap". If you want to provide a
    /// different debug name, manually use `GuestMemory::new` with
    /// [`AlignedHeapMemory`].
    pub fn allocate(size: usize) -> Self {
        Self::new_inner("heap".into(), AlignedHeapMemory::new(size), true)
    }

    /// If this memory is unaliased and was created via
    /// [`GuestMemory::allocate`], returns the backing buffer.
    ///
    /// Returns `Err(self)` if there are other references to this memory (via
    /// `clone()`).
    pub fn into_inner_buf(self) -> Result<AlignedHeapMemory, Self> {
        if !self.inner.allocated {
            return Err(self);
        }
        // FUTURE: consider using `Any` and `Arc::downcast` once trait upcasting is stable.
        // SAFETY: the inner implementation is guaranteed to be a `AlignedHeapMemory`.
        let inner = unsafe {
            Arc::<GuestMemoryInner<AlignedHeapMemory>>::from_raw(Arc::into_raw(self.inner).cast())
        };
        let inner = Arc::try_unwrap(inner).map_err(|inner| Self { inner })?;
        Ok(inner.imp)
    }

    /// If this memory was created via [`GuestMemory::allocate`], returns a slice to
    /// the allocated buffer.
    pub fn inner_buf(&self) -> Option<&[AtomicU8]> {
        if !self.inner.allocated {
            return None;
        }
        // FUTURE: consider using `<dyn Any>::downcast` once trait upcasting is stable.
        // SAFETY: the inner implementation is guaranteed to be a `AlignedHeapMemory`.
        let inner = unsafe { &*core::ptr::from_ref(&self.inner.imp).cast::<AlignedHeapMemory>() };
        Some(inner)
    }

    /// If this memory was created via [`GuestMemory::allocate`] and there are
    /// no other references to it, returns a mutable slice to the backing
    /// buffer.
    pub fn inner_buf_mut(&mut self) -> Option<&mut [u8]> {
        if !self.inner.allocated {
            return None;
        }
        let inner = Arc::get_mut(&mut self.inner)?;
        // FUTURE: consider using `<dyn Any>::downcast` once trait upcasting is stable.
        // SAFETY: the inner implementation is guaranteed to be a `AlignedHeapMemory`.
        let imp = unsafe { &mut *core::ptr::from_mut(&mut inner.imp).cast::<AlignedHeapMemory>() };
        Some(imp.as_mut())
    }

    /// Returns an empty guest memory, which fails every operation.
    pub fn empty() -> Self {
        GuestMemory::new("empty", Empty)
    }

    fn wrap_err(
        &self,
        gpa_len: Option<(u64, u64)>,
        op: GuestMemoryOperation,
        err: GuestMemoryBackingError,
    ) -> GuestMemoryError {
        let range = gpa_len.map(|(gpa, len)| gpa..gpa.wrapping_add(len));
        GuestMemoryError::new(&self.inner.debug_name, range, op, err)
    }

    fn with_op<T>(
        &self,
        gpa_len: Option<(u64, u64)>,
        op: GuestMemoryOperation,
        f: impl FnOnce() -> Result<T, GuestMemoryBackingError>,
    ) -> Result<T, GuestMemoryError> {
        f().map_err(|err| self.wrap_err(gpa_len, op, err))
    }

    /// Creates a smaller view into guest memory, constraining accesses within the new boundaries. For smaller ranges,
    /// some memory implementations (e.g. HDV) may choose to lock the pages into memory for faster access. Locking
    /// random guest memory may cause issues, so only opt in to this behavior when the range can be considered "owned"
    /// by the caller.
    pub fn subrange(
        &self,
        offset: u64,
        len: u64,
        allow_preemptive_locking: bool,
    ) -> Result<GuestMemory, GuestMemoryError> {
        self.with_op(Some((offset, len)), GuestMemoryOperation::Subrange, || {
            if let Some(guest_memory) =
                self.inner
                    .imp
                    .subrange(offset, len, allow_preemptive_locking)?
            {
                Ok(guest_memory)
            } else {
                create_memory_subrange(self.inner.clone(), offset, len, allow_preemptive_locking)
            }
        })
    }

    /// Returns a subrange where pages from the subrange can be locked.
    pub fn lockable_subrange(
        &self,
        offset: u64,
        len: u64,
    ) -> Result<GuestMemory, GuestMemoryError> {
        // TODO: Enforce subrange is actually lockable.
        self.subrange(offset, len, true)
    }

    /// Returns the mapping for all of guest memory.
    ///
    /// Returns `None` if there is more than one region or if the memory is not
    /// mapped.
    pub fn full_mapping(&self) -> Option<(*mut u8, usize)> {
        if let [region] = self.inner.regions.as_slice() {
            #[cfg(feature = "bitmap")]
            if region.bitmaps.is_some() {
                return None;
            }
            region
                .mapping
                .map(|SendPtrU8(ptr)| (ptr.as_ptr(), region.len as usize))
        } else {
            None
        }
    }

    /// Gets the IO address for DMAing to `gpa` from a user-mode driver not
    /// going through an IOMMU.
    pub fn iova(&self, gpa: u64) -> Option<u64> {
        let (region, offset, _) = self.inner.region(gpa, 1).ok()?;
        Some(region.base_iova? + offset)
    }

    /// Gets a pointer to the VA range for `gpa..gpa+len`.
    ///
    /// Returns `Ok(None)` if there is no mapping. Returns `Err(_)` if the
    /// memory is out of range.
    fn mapping_range(
        &self,
        access_type: AccessType,
        gpa: u64,
        len: usize,
    ) -> Result<Option<*mut u8>, GuestMemoryBackingError> {
        let (region, offset, _) = self.inner.region(gpa, len as u64)?;
        if let Some(SendPtrU8(ptr)) = region.mapping {
            loop {
                // SAFETY: offset + len is checked by `region()` to be inside the VA range.
                let fault_offset = unsafe {
                    match region.check_access(access_type, offset, len as u64) {
                        Ok(()) => return Ok(Some(ptr.as_ptr().add(offset as usize))),
                        Err(n) => n,
                    }
                };

                // Resolve the fault and try again.
                match self.inner.imp.page_fault(
                    gpa + fault_offset,
                    len - fault_offset as usize,
                    access_type == AccessType::Write,
                    true,
                ) {
                    PageFaultAction::Fail(err) => {
                        return Err(GuestMemoryBackingError::new(
                            err.kind,
                            gpa + fault_offset,
                            err.err,
                        ));
                    }
                    PageFaultAction::Retry => {}
                    PageFaultAction::Fallback => break,
                }
            }
        }
        Ok(None)
    }

    /// Runs `f` with a pointer to the mapped memory. If `f` fails, tries to
    /// resolve the fault (failing on error), then loops.
    ///
    /// If there is no mapping for the memory, or if the fault handler requests
    /// it, call `fallback` instead. `fallback` will not be called unless `gpa`
    /// and `len` are in range.
    fn run_on_mapping<T, P>(
        &self,
        access_type: AccessType,
        gpa: u64,
        len: usize,
        mut param: P,
        mut f: impl FnMut(&mut P, *mut u8) -> Result<T, trycopy::MemoryError>,
        fallback: impl FnOnce(&mut P) -> Result<T, GuestMemoryBackingError>,
    ) -> Result<T, GuestMemoryBackingError> {
        let op = || {
            let Some(mapping) = self.mapping_range(access_type, gpa, len)? else {
                return fallback(&mut param);
            };

            // Try until the fault fails to resolve.
            loop {
                match f(&mut param, mapping) {
                    Ok(t) => return Ok(t),
                    Err(fault) => {
                        match self.inner.imp.page_fault(
                            gpa + fault.offset() as u64,
                            len - fault.offset(),
                            access_type == AccessType::Write,
                            false,
                        ) {
                            PageFaultAction::Fail(err) => {
                                return Err(GuestMemoryBackingError::new(
                                    err.kind,
                                    gpa + fault.offset() as u64,
                                    err.err,
                                ));
                            }
                            PageFaultAction::Retry => {}
                            PageFaultAction::Fallback => return fallback(&mut param),
                        }
                    }
                }
            }
        };
        // If the `bitmap` feature is enabled, run the function in an RCU
        // critical section. This will allow callers to flush concurrent
        // accesses after bitmap updates.
        #[cfg(feature = "bitmap")]
        return rcu().run(op);
        #[cfg(not(feature = "bitmap"))]
        op()
    }

    /// # Safety
    ///
    /// The caller must ensure that `src`..`src + len` is a valid buffer for reads.
    unsafe fn write_ptr(
        &self,
        gpa: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        if len == 0 {
            return Ok(());
        }
        self.run_on_mapping(
            AccessType::Write,
            gpa,
            len,
            (),
            |(), dest| {
                // SAFETY: dest..dest+len is guaranteed to point to a reserved VA
                // range, and src..src+len is guaranteed by the caller to be a valid
                // buffer for reads.
                unsafe { trycopy::try_copy(src, dest, len) }
            },
            |()| {
                // SAFETY: src..src+len is guaranteed by the caller to point to a valid
                // buffer for reads.
                unsafe { self.inner.imp.write_fallback(gpa, src, len) }
            },
        )
    }

    /// Writes `src` into guest memory at address `gpa`.
    pub fn write_at(&self, gpa: u64, src: &[u8]) -> Result<(), GuestMemoryError> {
        self.with_op(
            Some((gpa, src.len() as u64)),
            GuestMemoryOperation::Write,
            || self.write_at_inner(gpa, src),
        )
    }

    fn write_at_inner(&self, gpa: u64, src: &[u8]) -> Result<(), GuestMemoryBackingError> {
        // SAFETY: `src` is a valid buffer for reads.
        unsafe { self.write_ptr(gpa, src.as_ptr(), src.len()) }
    }

    /// Writes `src` into guest memory at address `gpa`.
    pub fn write_from_atomic(&self, gpa: u64, src: &[AtomicU8]) -> Result<(), GuestMemoryError> {
        self.with_op(
            Some((gpa, src.len() as u64)),
            GuestMemoryOperation::Write,
            || {
                // SAFETY: `src` is a valid buffer for reads.
                unsafe { self.write_ptr(gpa, src.as_ptr().cast(), src.len()) }
            },
        )
    }

    /// Writes `len` bytes of `val` into guest memory at address `gpa`.
    pub fn fill_at(&self, gpa: u64, val: u8, len: usize) -> Result<(), GuestMemoryError> {
        self.with_op(Some((gpa, len as u64)), GuestMemoryOperation::Fill, || {
            self.fill_at_inner(gpa, val, len)
        })
    }

    fn fill_at_inner(&self, gpa: u64, val: u8, len: usize) -> Result<(), GuestMemoryBackingError> {
        if len == 0 {
            return Ok(());
        }
        self.run_on_mapping(
            AccessType::Write,
            gpa,
            len,
            (),
            |(), dest| {
                // SAFETY: dest..dest+len is guaranteed to point to a reserved VA range.
                unsafe { trycopy::try_write_bytes(dest, val, len) }
            },
            |()| self.inner.imp.fill_fallback(gpa, val, len),
        )
    }

    /// Reads from guest memory into `dest..dest+len`.
    ///
    /// # Safety
    /// The caller must ensure dest..dest+len is a valid buffer for writes.
    unsafe fn read_ptr(
        &self,
        gpa: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        if len == 0 {
            return Ok(());
        }
        self.run_on_mapping(
            AccessType::Read,
            gpa,
            len,
            (),
            |(), src| {
                // SAFETY: src..src+len is guaranteed to point to a reserved VA
                // range, and dest..dest+len is guaranteed by the caller to be a
                // valid buffer for writes.
                unsafe { trycopy::try_copy(src, dest, len) }
            },
            |()| {
                // SAFETY: dest..dest+len is guaranteed by the caller to point to a
                // valid buffer for writes.
                unsafe { self.inner.imp.read_fallback(gpa, dest, len) }
            },
        )
    }

    fn read_at_inner(&self, gpa: u64, dest: &mut [u8]) -> Result<(), GuestMemoryBackingError> {
        // SAFETY: `dest` is a valid buffer for writes.
        unsafe { self.read_ptr(gpa, dest.as_mut_ptr(), dest.len()) }
    }

    /// Reads from guest memory address `gpa` into `dest`.
    pub fn read_at(&self, gpa: u64, dest: &mut [u8]) -> Result<(), GuestMemoryError> {
        self.with_op(
            Some((gpa, dest.len() as u64)),
            GuestMemoryOperation::Read,
            || self.read_at_inner(gpa, dest),
        )
    }

    /// Reads from guest memory address `gpa` into `dest`.
    pub fn read_to_atomic(&self, gpa: u64, dest: &[AtomicU8]) -> Result<(), GuestMemoryError> {
        self.with_op(
            Some((gpa, dest.len() as u64)),
            GuestMemoryOperation::Read,
            // SAFETY: `dest` is a valid buffer for writes.
            || unsafe { self.read_ptr(gpa, dest.as_ptr() as *mut u8, dest.len()) },
        )
    }

    /// Writes an object to guest memory at address `gpa`.
    ///
    /// If the object is 1, 2, 4, or 8 bytes and the address is naturally
    /// aligned, then the write will be performed atomically. Here, this means
    /// that concurrent readers (via `read_plain`) cannot observe a torn write
    /// but will observe either the old or new value.
    ///
    /// The memory ordering of the write is unspecified.
    ///
    /// FUTURE: once we are on Rust 1.79, add a method specifically for atomic
    /// accesses that const asserts that the size is appropriate.
    pub fn write_plain<T: IntoBytes + Immutable + KnownLayout>(
        &self,
        gpa: u64,
        b: &T,
    ) -> Result<(), GuestMemoryError> {
        // Note that this is const, so the match below will compile out.
        let len = size_of::<T>();
        self.with_op(Some((gpa, len as u64)), GuestMemoryOperation::Write, || {
            self.run_on_mapping(
                AccessType::Write,
                gpa,
                len,
                (),
                |(), dest| {
                    // SAFETY: dest..dest+len is guaranteed to point to
                    // a reserved VA range.
                    unsafe { trycopy::try_write_volatile(dest.cast(), b) }
                },
                |()| {
                    // SAFETY: b is a valid buffer for reads.
                    unsafe {
                        self.inner
                            .imp
                            .write_fallback(gpa, b.as_bytes().as_ptr(), len)
                    }
                },
            )
        })
    }

    /// Attempts a sequentially-consistent compare exchange of the value at `gpa`.
    pub fn compare_exchange<T: IntoBytes + FromBytes + Immutable + KnownLayout + Copy>(
        &self,
        gpa: u64,
        current: T,
        new: T,
    ) -> Result<Result<T, T>, GuestMemoryError> {
        const {
            assert!(matches!(size_of::<T>(), 1 | 2 | 4 | 8));
            assert!(align_of::<T>() >= size_of::<T>());
        };
        let len = size_of_val(&new);
        self.with_op(
            Some((gpa, len as u64)),
            GuestMemoryOperation::CompareExchange,
            || {
                // Assume that if write is allowed, then read is allowed.
                self.run_on_mapping(
                    AccessType::Write,
                    gpa,
                    len,
                    (),
                    |(), dest| {
                        // SAFETY: dest..dest+len is guaranteed by the caller to be a valid
                        // buffer for writes.
                        unsafe { trycopy::try_compare_exchange(dest.cast(), current, new) }
                    },
                    |()| {
                        let mut current = current;
                        let success = self.inner.imp.compare_exchange_fallback(
                            gpa,
                            current.as_mut_bytes(),
                            new.as_bytes(),
                        )?;

                        Ok(if success { Ok(new) } else { Err(current) })
                    },
                )
            },
        )
    }

    /// Reads an object from guest memory at address `gpa`.
    ///
    /// If the object is 1, 2, 4, or 8 bytes and the address is naturally
    /// aligned, then the read will be performed atomically. Here, this means
    /// that when there is a concurrent writer, callers will observe either the
    /// old or new value, but not a torn read.
    ///
    /// The memory ordering of the read is unspecified.
    ///
    /// FUTURE: once we are on Rust 1.79, add a method specifically for atomic
    /// accesses that const asserts that the size is appropriate.
    pub fn read_plain<T: FromBytes + Immutable + KnownLayout>(
        &self,
        gpa: u64,
    ) -> Result<T, GuestMemoryError> {
        self.with_op(
            Some((gpa, size_of::<T>() as u64)),
            GuestMemoryOperation::Read,
            || self.read_plain_inner(gpa),
        )
    }

    fn read_plain_inner<T: FromBytes + Immutable + KnownLayout>(
        &self,
        gpa: u64,
    ) -> Result<T, GuestMemoryBackingError> {
        let len = size_of::<T>();
        self.run_on_mapping(
            AccessType::Read,
            gpa,
            len,
            (),
            |(), src| {
                // SAFETY: src..src+len is guaranteed to point to a reserved VA
                // range.
                unsafe { trycopy::try_read_volatile(src.cast::<T>()) }
            },
            |()| {
                let mut obj = std::mem::MaybeUninit::<T>::zeroed();
                // SAFETY: dest..dest+len is guaranteed by the caller to point to a
                // valid buffer for writes.
                unsafe {
                    self.inner
                        .imp
                        .read_fallback(gpa, obj.as_mut_ptr().cast(), len)?;
                }
                // SAFETY: `obj` was fully initialized by `read_fallback`.
                Ok(unsafe { obj.assume_init() })
            },
        )
    }

    fn probe_page_for_lock(
        &self,
        with_kernel_access: bool,
        gpa: u64,
    ) -> Result<*const AtomicU8, GuestMemoryBackingError> {
        let (region, offset, _) = self.inner.region(gpa, 1)?;
        let Some(SendPtrU8(ptr)) = region.mapping else {
            return Err(GuestMemoryBackingError::other(gpa, NotLockable));
        };
        // Ensure the virtual address can be exposed.
        if with_kernel_access {
            self.inner.imp.expose_va(gpa, 1)?;
        }
        // FUTURE: check the correct bitmap for the access type, which needs to
        // be passed in.
        self.read_plain_inner::<u8>(gpa)?;
        // SAFETY: the read_at call includes a check that ensures that
        // `gpa` is in the VA range.
        let page = unsafe { ptr.as_ptr().add(offset as usize) };
        Ok(page.cast())
    }

    pub fn lock_gpns(
        &self,
        with_kernel_access: bool,
        gpns: &[u64],
    ) -> Result<LockedPages, GuestMemoryError> {
        self.with_op(None, GuestMemoryOperation::Lock, || {
            let mut pages = Vec::with_capacity(gpns.len());
            for &gpn in gpns {
                let gpa = gpn_to_gpa(gpn).map_err(GuestMemoryBackingError::gpn)?;
                let page = self.probe_page_for_lock(with_kernel_access, gpa)?;
                pages.push(PagePtr(page));
            }
            let store_gpns = self.inner.imp.lock_gpns(gpns)?;
            Ok(LockedPages {
                pages: pages.into_boxed_slice(),
                gpns: store_gpns.then(|| gpns.to_vec().into_boxed_slice()),
                mem: self.inner.clone(),
            })
        })
    }

    pub fn probe_gpns(&self, gpns: &[u64]) -> Result<(), GuestMemoryError> {
        self.with_op(None, GuestMemoryOperation::Probe, || {
            for &gpn in gpns {
                self.read_plain_inner::<u8>(
                    gpn_to_gpa(gpn).map_err(GuestMemoryBackingError::gpn)?,
                )?;
            }
            Ok(())
        })
    }

    /// Check if a given GPA is readable or not.
    pub fn probe_gpa_readable(&self, gpa: u64) -> Result<(), GuestMemoryErrorKind> {
        let mut b = [0];
        self.read_at_inner(gpa, &mut b).map_err(|err| err.kind)
    }

    /// Check if a given GPA is writeable or not.
    pub fn probe_gpa_writable(&self, gpa: u64) -> Result<(), GuestMemoryErrorKind> {
        let _ = self
            .compare_exchange(gpa, 0u8, 0)
            .map_err(|err| err.kind())?;
        Ok(())
    }

    /// Gets a slice of guest memory assuming the memory was already locked via
    /// [`GuestMemory::lock_gpns`].
    ///
    /// This is dangerous--if the pages have not been locked, then it could
    /// cause an access violation or guest memory corruption.
    ///
    /// Note that this is not `unsafe` since this cannot cause memory corruption
    /// in this process. Even if there is an access violation, the underlying VA
    /// space is known to be reserved.
    ///
    /// Panics if the requested buffer is out of range.
    fn dangerous_access_pre_locked_memory(&self, gpa: u64, len: usize) -> &[AtomicU8] {
        let addr = self
            .mapping_range(AccessType::Write, gpa, len)
            .unwrap()
            .unwrap();
        // SAFETY: addr..addr+len is checked above to be a valid VA range. It's
        // possible some of the pages aren't mapped and will cause AVs at
        // runtime when accessed, but, as discussed above, at a language level
        // this cannot cause any safety issues.
        unsafe { std::slice::from_raw_parts(addr.cast(), len) }
    }

    fn op_range<F: FnMut(u64, Range<usize>) -> Result<(), GuestMemoryBackingError>>(
        &self,
        op: GuestMemoryOperation,
        range: &PagedRange<'_>,
        mut f: F,
    ) -> Result<(), GuestMemoryError> {
        self.with_op(None, op, || {
            let gpns = range.gpns();
            let offset = range.offset();

            // Perform the operation in three phases: the first page (if it is not a
            // full page), the full pages, and the last page (if it is not a full
            // page).
            let mut byte_index = 0;
            let mut len = range.len();
            let mut page = 0;
            if !offset.is_multiple_of(PAGE_SIZE) {
                let head_len = std::cmp::min(len, PAGE_SIZE - (offset % PAGE_SIZE));
                let addr = gpn_to_gpa(gpns[page]).map_err(GuestMemoryBackingError::gpn)?
                    + offset as u64 % PAGE_SIZE64;
                f(addr, byte_index..byte_index + head_len)?;
                byte_index += head_len;
                len -= head_len;
                page += 1;
            }
            while len >= PAGE_SIZE {
                f(
                    gpn_to_gpa(gpns[page]).map_err(GuestMemoryBackingError::gpn)?,
                    byte_index..byte_index + PAGE_SIZE,
                )?;
                byte_index += PAGE_SIZE;
                len -= PAGE_SIZE;
                page += 1;
            }
            if len > 0 {
                f(
                    gpn_to_gpa(gpns[page]).map_err(GuestMemoryBackingError::gpn)?,
                    byte_index..byte_index + len,
                )?;
            }

            Ok(())
        })
    }

    pub fn write_range(&self, range: &PagedRange<'_>, data: &[u8]) -> Result<(), GuestMemoryError> {
        assert!(data.len() == range.len());
        self.op_range(GuestMemoryOperation::Write, range, move |addr, r| {
            self.write_at_inner(addr, &data[r])
        })
    }

    pub fn fill_range(&self, range: &PagedRange<'_>, val: u8) -> Result<(), GuestMemoryError> {
        self.op_range(GuestMemoryOperation::Fill, range, move |addr, r| {
            self.fill_at_inner(addr, val, r.len())
        })
    }

    pub fn zero_range(&self, range: &PagedRange<'_>) -> Result<(), GuestMemoryError> {
        self.op_range(GuestMemoryOperation::Fill, range, move |addr, r| {
            self.fill_at_inner(addr, 0, r.len())
        })
    }

    pub fn read_range(
        &self,
        range: &PagedRange<'_>,
        data: &mut [u8],
    ) -> Result<(), GuestMemoryError> {
        assert!(data.len() == range.len());
        self.op_range(GuestMemoryOperation::Read, range, move |addr, r| {
            self.read_at_inner(addr, &mut data[r])
        })
    }

    pub fn write_range_from_atomic(
        &self,
        range: &PagedRange<'_>,
        data: &[AtomicU8],
    ) -> Result<(), GuestMemoryError> {
        assert!(data.len() == range.len());
        self.op_range(GuestMemoryOperation::Write, range, move |addr, r| {
            let src = &data[r];
            // SAFETY: `src` is a valid buffer for reads.
            unsafe { self.write_ptr(addr, src.as_ptr().cast(), src.len()) }
        })
    }

    pub fn read_range_to_atomic(
        &self,
        range: &PagedRange<'_>,
        data: &[AtomicU8],
    ) -> Result<(), GuestMemoryError> {
        assert!(data.len() == range.len());
        self.op_range(GuestMemoryOperation::Read, range, move |addr, r| {
            let dest = &data[r];
            // SAFETY: `dest` is a valid buffer for writes.
            unsafe { self.read_ptr(addr, dest.as_ptr().cast_mut().cast(), dest.len()) }
        })
    }

    /// Locks the guest pages spanned by the specified `PagedRange` for the `'static` lifetime.
    ///
    /// # Arguments
    /// * 'paged_range' - The guest memory range to lock.
    /// * 'locked_range' - Receives a list of VA ranges to which each contiguous physical sub-range in `paged_range`
    ///   has been mapped. Must be initially empty.
    pub fn lock_range<T: LockedRange>(
        &self,
        paged_range: PagedRange<'_>,
        mut locked_range: T,
    ) -> Result<LockedRangeImpl<T>, GuestMemoryError> {
        self.with_op(None, GuestMemoryOperation::Lock, || {
            let gpns = paged_range.gpns();
            for &gpn in gpns {
                let gpa = gpn_to_gpa(gpn).map_err(GuestMemoryBackingError::gpn)?;
                self.probe_page_for_lock(true, gpa)?;
            }
            for range in paged_range.ranges() {
                let range = range.map_err(GuestMemoryBackingError::gpn)?;
                locked_range.push_sub_range(
                    self.dangerous_access_pre_locked_memory(range.start, range.len() as usize),
                );
            }
            let store_gpns = self.inner.imp.lock_gpns(paged_range.gpns())?;
            Ok(LockedRangeImpl {
                mem: self.inner.clone(),
                gpns: store_gpns.then(|| paged_range.gpns().to_vec().into_boxed_slice()),
                inner: locked_range,
            })
        })
    }
}

#[derive(Debug, Error)]
#[error("invalid guest page number {0:#x}")]
pub struct InvalidGpn(u64);

fn gpn_to_gpa(gpn: u64) -> Result<u64, InvalidGpn> {
    gpn.checked_mul(PAGE_SIZE64).ok_or(InvalidGpn(gpn))
}

#[derive(Debug, Copy, Clone, Default)]
struct RegionDefinition {
    invalid_mask: u64,
    region_mask: u64,
    region_bits: u32,
}

impl RegionDefinition {
    fn region(&self, gpa: u64, len: u64) -> Result<(usize, u64), GuestMemoryBackingError> {
        if (gpa | len) & self.invalid_mask != 0 {
            return Err(GuestMemoryBackingError::new(
                GuestMemoryErrorKind::OutOfRange,
                gpa,
                OutOfRange,
            ));
        }
        let offset = gpa & self.region_mask;
        if offset.wrapping_add(len) & !self.region_mask != 0 {
            return Err(GuestMemoryBackingError::new(
                GuestMemoryErrorKind::OutOfRange,
                gpa,
                OutOfRange,
            ));
        }
        let index = (gpa >> self.region_bits) as usize;
        Ok((index, offset))
    }
}

impl GuestMemoryInner {
    fn region(
        &self,
        gpa: u64,
        len: u64,
    ) -> Result<(&MemoryRegion, u64, usize), GuestMemoryBackingError> {
        let (index, offset) = self.region_def.region(gpa, len)?;
        let region = &self.regions[index];
        if offset + len > region.len {
            return Err(GuestMemoryBackingError::new(
                GuestMemoryErrorKind::OutOfRange,
                gpa,
                OutOfRange,
            ));
        }
        Ok((&self.regions[index], offset, index))
    }
}

#[derive(Clone)]
pub struct LockedPages {
    pages: Box<[PagePtr]>,
    gpns: Option<Box<[u64]>>,
    // maintain a reference to the backing memory
    mem: Arc<GuestMemoryInner>,
}

impl Drop for LockedPages {
    fn drop(&mut self) {
        if let Some(gpns) = &self.gpns {
            self.mem.imp.unlock_gpns(gpns);
        }
    }
}

impl Debug for LockedPages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockedPages")
            .field("page_count", &self.pages.len())
            .finish()
    }
}

#[derive(Copy, Clone, Debug)]
// Field is read via slice transmute and pointer casts, not actually dead.
struct PagePtr(#[expect(dead_code)] *const AtomicU8);

// SAFETY: PagePtr is just a pointer with no methods and has no inherent safety
// constraints.
unsafe impl Send for PagePtr {}
// SAFETY: see above comment
unsafe impl Sync for PagePtr {}

pub type Page = [AtomicU8; PAGE_SIZE];

impl LockedPages {
    #[inline]
    pub fn pages(&self) -> &[&Page] {
        // SAFETY: PagePtr is just a pointer to a Page. The pages are kept alive by
        // the reference in _mem, and the lifetimes here ensure the LockedPages outlives
        // the slice.
        unsafe { std::slice::from_raw_parts(self.pages.as_ptr().cast::<&Page>(), self.pages.len()) }
    }
}

impl<'a> AsRef<[&'a Page]> for &'a LockedPages {
    fn as_ref(&self) -> &[&'a Page] {
        self.pages()
    }
}

/// Represents a range of locked guest pages as an ordered list of the VA sub-ranges
/// to which the guest pages are mapped.
/// The range may only partially span the first and last page and must fully span all
/// intermediate pages.
pub trait LockedRange {
    /// Adds a sub-range to this range.
    fn push_sub_range(&mut self, sub_range: &[AtomicU8]);
}

pub struct LockedRangeImpl<T: LockedRange> {
    mem: Arc<GuestMemoryInner>,
    gpns: Option<Box<[u64]>>,
    inner: T,
}

impl<T: LockedRange> LockedRangeImpl<T> {
    pub fn get(&self) -> &T {
        &self.inner
    }
}

impl<T: LockedRange> Drop for LockedRangeImpl<T> {
    fn drop(&mut self) {
        if let Some(gpns) = &self.gpns {
            self.mem.imp.unlock_gpns(gpns);
        }
    }
}

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
        let (dest, rest) = std::mem::take(self).split_at_mut(data.len());
        dest.copy_from_slice(data);
        *self = rest;
        Ok(())
    }

    fn fill(&mut self, val: u8, len: usize) -> Result<(), AccessError> {
        if self.len() < len {
            return Err(AccessError::OutOfRange(self.len(), len));
        }
        let (dest, rest) = std::mem::take(self).split_at_mut(len);
        dest.fill(val);
        *self = rest;
        Ok(())
    }

    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

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

/// Trait implemented to allow mapping and unmapping a region of memory at
/// a particular guest address.
pub trait MappableGuestMemory: Send + Sync {
    /// Maps the memory into the guest.
    ///
    /// `writable` specifies whether the guest can write to the memory region.
    /// If a guest tries to write to a non-writable region, the virtual
    /// processor will exit for MMIO handling.
    fn map_to_guest(&mut self, gpa: u64, writable: bool) -> io::Result<()>;

    fn unmap_from_guest(&mut self);
}

/// Trait implemented for a region of memory that can have memory mapped into
/// it.
pub trait MappedMemoryRegion: Send + Sync {
    /// Maps an object at `offset` in the region.
    ///
    /// Behaves like mmap--overwrites and splits existing mappings.
    fn map(
        &self,
        offset: usize,
        section: &dyn AsMappableRef,
        file_offset: u64,
        len: usize,
        writable: bool,
    ) -> io::Result<()>;

    /// Unmaps any mappings in the specified range within the region.
    fn unmap(&self, offset: usize, len: usize) -> io::Result<()>;
}

/// Trait implemented to allow the creation of memory regions.
pub trait MemoryMapper: Send + Sync {
    /// Creates a new memory region that can later be mapped into the guest.
    ///
    /// Returns both an interface for mapping/unmapping the region and for
    /// adding internal mappings.
    fn new_region(
        &self,
        len: usize,
        debug_name: String,
    ) -> io::Result<(Box<dyn MappableGuestMemory>, Arc<dyn MappedMemoryRegion>)>;
}

/// Doorbell provides a mechanism to register for notifications on writes to specific addresses in guest memory.
pub trait DoorbellRegistration: Send + Sync {
    /// Register a doorbell event.
    fn register_doorbell(
        &self,
        guest_address: u64,
        value: Option<u64>,
        length: Option<u32>,
        event: &Event,
    ) -> io::Result<Box<dyn Send + Sync>>;
}

/// Trait to map a ROM at one or more locations in guest memory.
pub trait MapRom: Send + Sync {
    /// Maps the specified portion of the ROM into guest memory at `gpa`.
    ///
    /// The returned object will implicitly unmap the ROM when dropped.
    fn map_rom(&self, gpa: u64, offset: u64, len: u64) -> io::Result<Box<dyn UnmapRom>>;

    /// Returns the length of the ROM in bytes.
    fn len(&self) -> u64;
}

/// Trait to unmap a ROM from guest memory.
pub trait UnmapRom: Send + Sync {
    /// Unmaps the ROM from guest memory.
    fn unmap_rom(self);
}

#[cfg(test)]
#[expect(clippy::undocumented_unsafe_blocks)]
mod tests {
    use crate::GuestMemory;
    use crate::PAGE_SIZE64;
    use crate::PageFaultAction;
    use crate::PageFaultError;
    use sparse_mmap::SparseMapping;
    use std::ptr::NonNull;
    use std::sync::Arc;
    use thiserror::Error;

    /// An implementation of a GuestMemoryAccess trait that expects all of
    /// guest memory to be mapped at a given base, with mmap or the Windows
    /// equivalent. Pages that are not backed by RAM will return failure
    /// when attempting to access them.
    pub struct GuestMemoryMapping {
        mapping: SparseMapping,
        #[cfg(feature = "bitmap")]
        bitmap: Option<Vec<u8>>,
    }

    unsafe impl crate::GuestMemoryAccess for GuestMemoryMapping {
        fn mapping(&self) -> Option<NonNull<u8>> {
            NonNull::new(self.mapping.as_ptr().cast())
        }

        fn max_address(&self) -> u64 {
            self.mapping.len() as u64
        }

        #[cfg(feature = "bitmap")]
        fn access_bitmap(&self) -> Option<crate::BitmapInfo> {
            self.bitmap.as_ref().map(|bm| crate::BitmapInfo {
                read_bitmap: NonNull::new(bm.as_ptr().cast_mut()).unwrap(),
                write_bitmap: NonNull::new(bm.as_ptr().cast_mut()).unwrap(),
                bit_offset: 0,
            })
        }
    }

    const PAGE_SIZE: usize = 4096;
    const SIZE_1MB: usize = 1048576;

    /// Create a test guest layout:
    /// 0           -> 1MB          RAM
    /// 1MB         -> 2MB          empty
    /// 2MB         -> 3MB          RAM
    /// 3MB         -> 3MB + 4K     empty
    /// 3MB + 4K    -> 4MB          RAM
    fn create_test_mapping() -> GuestMemoryMapping {
        let mapping = SparseMapping::new(SIZE_1MB * 4).unwrap();
        mapping.alloc(0, SIZE_1MB).unwrap();
        mapping.alloc(2 * SIZE_1MB, SIZE_1MB).unwrap();
        mapping
            .alloc(3 * SIZE_1MB + PAGE_SIZE, SIZE_1MB - PAGE_SIZE)
            .unwrap();

        GuestMemoryMapping {
            mapping,
            #[cfg(feature = "bitmap")]
            bitmap: None,
        }
    }

    #[test]
    fn test_basic_read_write() {
        let mapping = create_test_mapping();
        let gm = GuestMemory::new("test", mapping);

        // Test reading at 0.
        let addr = 0;
        let result = gm.read_plain::<u8>(addr);
        assert_eq!(result.unwrap(), 0);

        // Test read/write to first page
        let write_buffer = [1, 2, 3, 4, 5];
        let mut read_buffer = [0; 5];
        gm.write_at(0, &write_buffer).unwrap();
        gm.read_at(0, &mut read_buffer).unwrap();
        assert_eq!(write_buffer, read_buffer);
        assert_eq!(gm.read_plain::<u8>(0).unwrap(), 1);
        assert_eq!(gm.read_plain::<u8>(1).unwrap(), 2);
        assert_eq!(gm.read_plain::<u8>(2).unwrap(), 3);
        assert_eq!(gm.read_plain::<u8>(3).unwrap(), 4);
        assert_eq!(gm.read_plain::<u8>(4).unwrap(), 5);

        // Test read/write to page at 2MB
        let addr = 2 * SIZE_1MB as u64;
        let write_buffer: Vec<u8> = (0..PAGE_SIZE).map(|x| x as u8).collect();
        let mut read_buffer: Vec<u8> = (0..PAGE_SIZE).map(|_| 0).collect();
        gm.write_at(addr, write_buffer.as_slice()).unwrap();
        gm.read_at(addr, read_buffer.as_mut_slice()).unwrap();
        assert_eq!(write_buffer, read_buffer);

        // Test read/write to first 1MB
        let write_buffer: Vec<u8> = (0..SIZE_1MB).map(|x| x as u8).collect();
        let mut read_buffer: Vec<u8> = (0..SIZE_1MB).map(|_| 0).collect();
        gm.write_at(addr, write_buffer.as_slice()).unwrap();
        gm.read_at(addr, read_buffer.as_mut_slice()).unwrap();
        assert_eq!(write_buffer, read_buffer);

        // Test bad read at 1MB
        let addr = SIZE_1MB as u64;
        let result = gm.read_plain::<u8>(addr);
        assert!(result.is_err());
    }

    #[test]
    fn test_multi() {
        let len = SIZE_1MB * 4;
        let mapping = SparseMapping::new(len).unwrap();
        mapping.alloc(0, len).unwrap();
        let mapping = Arc::new(GuestMemoryMapping {
            mapping,
            #[cfg(feature = "bitmap")]
            bitmap: None,
        });
        let region_len = 1 << 30;
        let gm = GuestMemory::new_multi_region(
            "test",
            region_len,
            vec![Some(mapping.clone()), None, Some(mapping.clone())],
        )
        .unwrap();

        let mut b = [0];
        let len = len as u64;
        gm.read_at(0, &mut b).unwrap();
        gm.read_at(len, &mut b).unwrap_err();
        gm.read_at(region_len, &mut b).unwrap_err();
        gm.read_at(2 * region_len, &mut b).unwrap();
        gm.read_at(2 * region_len + len, &mut b).unwrap_err();
        gm.read_at(3 * region_len, &mut b).unwrap_err();
    }

    #[cfg(feature = "bitmap")]
    #[test]
    fn test_bitmap() {
        let len = PAGE_SIZE * 4;
        let mapping = SparseMapping::new(len).unwrap();
        mapping.alloc(0, len).unwrap();
        let bitmap = vec![0b0101];
        let mapping = Arc::new(GuestMemoryMapping {
            mapping,
            bitmap: Some(bitmap),
        });
        let gm = GuestMemory::new("test", mapping);

        gm.read_plain::<[u8; 1]>(0).unwrap();
        gm.read_plain::<[u8; 1]>(PAGE_SIZE64 - 1).unwrap();
        gm.read_plain::<[u8; 2]>(PAGE_SIZE64 - 1).unwrap_err();
        gm.read_plain::<[u8; 1]>(PAGE_SIZE64).unwrap_err();
        gm.read_plain::<[u8; 1]>(PAGE_SIZE64 * 2).unwrap();
        gm.read_plain::<[u8; PAGE_SIZE * 2]>(0).unwrap_err();
    }

    struct FaultingMapping {
        mapping: SparseMapping,
    }

    #[derive(Debug, Error)]
    #[error("fault")]
    struct Fault;

    unsafe impl crate::GuestMemoryAccess for FaultingMapping {
        fn mapping(&self) -> Option<NonNull<u8>> {
            NonNull::new(self.mapping.as_ptr().cast())
        }

        fn max_address(&self) -> u64 {
            self.mapping.len() as u64
        }

        fn page_fault(
            &self,
            address: u64,
            _len: usize,
            write: bool,
            bitmap_failure: bool,
        ) -> PageFaultAction {
            assert!(!bitmap_failure);
            let qlen = self.mapping.len() as u64 / 4;
            if address < qlen || address >= 3 * qlen {
                return PageFaultAction::Fail(PageFaultError::other(Fault));
            }
            let page_address = (address as usize) & !(PAGE_SIZE - 1);
            if address >= 2 * qlen {
                if write {
                    return PageFaultAction::Fail(PageFaultError::other(Fault));
                }
                self.mapping.map_zero(page_address, PAGE_SIZE).unwrap();
            } else {
                self.mapping.alloc(page_address, PAGE_SIZE).unwrap();
            }
            PageFaultAction::Retry
        }
    }

    impl FaultingMapping {
        fn new(len: usize) -> Self {
            let mapping = SparseMapping::new(len).unwrap();
            FaultingMapping { mapping }
        }
    }

    #[test]
    fn test_fault() {
        let len = PAGE_SIZE * 4;
        let mapping = FaultingMapping::new(len);
        let gm = GuestMemory::new("test", mapping);

        gm.write_plain::<u8>(0, &0).unwrap_err();
        gm.read_plain::<u8>(PAGE_SIZE64 - 1).unwrap_err();
        gm.read_plain::<u8>(PAGE_SIZE64).unwrap();
        gm.write_plain::<u8>(PAGE_SIZE64, &0).unwrap();
        gm.write_plain::<u16>(PAGE_SIZE64 * 3 - 1, &0).unwrap_err();
        gm.read_plain::<u16>(PAGE_SIZE64 * 3 - 1).unwrap_err();
        gm.read_plain::<u8>(PAGE_SIZE64 * 3 - 1).unwrap();
        gm.write_plain::<u8>(PAGE_SIZE64 * 3 - 1, &0).unwrap_err();
    }

    #[test]
    fn test_allocated() {
        let mut gm = GuestMemory::allocate(0x10000);
        let pattern = [0x42; 0x10000];
        gm.write_at(0, &pattern).unwrap();
        assert_eq!(gm.inner_buf_mut().unwrap(), &pattern);
        gm.inner_buf().unwrap();
        let gm2 = gm.clone();
        assert!(gm.inner_buf_mut().is_none());
        gm.inner_buf().unwrap();
        let mut gm = gm.into_inner_buf().unwrap_err();
        drop(gm2);
        assert_eq!(gm.inner_buf_mut().unwrap(), &pattern);
        gm.into_inner_buf().unwrap();
    }
}
