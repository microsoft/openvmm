// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Traits and types for sharing host memory with the device.

use safeatomic::AtomicSliceOps;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// The 4KB page size used by user-mode devices.
pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SIZE32: u32 = 4096;
pub const PAGE_SIZE64: u64 = PAGE_SIZE as u64;

/// A mapped buffer that can be accessed by the host or the device.
///
/// # Safety
/// The implementor must ensure that the VA region from `base()..base() + len()`
/// remains mapped for the lifetime.
pub unsafe trait MappedDmaTarget: Send + Sync {
    /// The virtual address of the mapped memory.
    fn base(&self) -> *const u8;

    /// The length of the buffer in bytes.
    fn len(&self) -> usize;

    /// 4KB page numbers used to refer to the memory when communicating with the
    /// device.
    fn pfns(&self) -> &[u64];

    /// The pfn_bias on confidential platforms (aka vTOM) applied to PFNs in [`Self::pfns()`],
    fn pfn_bias(&self) -> u64;

    /// Returns a view of a subset of the buffer.
    ///
    /// Returns `None` if the default implementation should be used.
    ///
    /// This should not be implemented except by internal implementations.
    #[doc(hidden)]
    fn view(&self, offset: usize, len: usize) -> Option<MemoryBlock> {
        let _ = (offset, len);
        None
    }
}

struct RestrictedView {
    mem: Arc<dyn MappedDmaTarget>,
    len: usize,
    offset: usize,
}

impl RestrictedView {
    /// Wraps `mem` and provides a restricted view of it.
    fn new(mem: Arc<dyn MappedDmaTarget>, offset: usize, len: usize) -> Self {
        let mem_len = mem.len();
        assert!(mem_len >= offset && mem_len - offset >= len);
        Self { len, offset, mem }
    }
}

// SAFETY: Passing through to the underlying impl after restricting the bounds
// (which were validated in `new`).
unsafe impl MappedDmaTarget for RestrictedView {
    fn base(&self) -> *const u8 {
        // SAFETY: verified in `new` to be in bounds.
        unsafe { self.mem.base().add(self.offset) }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn pfns(&self) -> &[u64] {
        let start = self.offset / PAGE_SIZE;
        let count = (self.base() as usize % PAGE_SIZE + self.len + 0xfff) / PAGE_SIZE;
        let pages = self.mem.pfns();
        &pages[start..][..count]
    }

    fn pfn_bias(&self) -> u64 {
        self.mem.pfn_bias()
    }

    fn view(&self, offset: usize, len: usize) -> Option<MemoryBlock> {
        Some(MemoryBlock::new(RestrictedView::new(
            self.mem.clone(),
            self.offset.checked_add(offset).unwrap(),
            len,
        )))
    }
}

/// A DMA target.
#[derive(Clone)]
pub struct MemoryBlock {
    base: *const u8,
    len: usize,
    mem: Arc<dyn MappedDmaTarget>,
}

impl std::fmt::Debug for MemoryBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryBlock")
            .field("base", &self.base)
            .field("len", &self.len)
            .field("pfns", &self.pfns())
            .field("pfn_bias", &self.pfn_bias())
            .finish()
    }
}

// SAFETY: The inner MappedDmaTarget is Send + Sync, so a view of it is too.
unsafe impl Send for MemoryBlock {}
// SAFETY: The inner MappedDmaTarget is Send + Sync, so a view of it is too.
unsafe impl Sync for MemoryBlock {}

impl MemoryBlock {
    /// Creates a new memory block backed by `mem`.
    pub fn new<T: 'static + MappedDmaTarget>(mem: T) -> Self {
        Self {
            base: mem.base(),
            len: mem.len(),
            mem: Arc::new(mem),
        }
    }

    /// Returns a view of a subset of the buffer.
    pub fn subblock(&self, offset: usize, len: usize) -> Self {
        match self.mem.view(offset, len) {
            Some(view) => view,
            None => Self::new(RestrictedView::new(self.mem.clone(), offset, len)),
        }
    }

    /// Get the base address of the buffer.
    pub fn base(&self) -> *const u8 {
        self.base
    }

    /// Gets the length of the buffer in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Gets the PFNs of the underlying memory.
    pub fn pfns(&self) -> &[u64] {
        self.mem.pfns()
    }

    /// Gets the pfn_bias of the underlying memory.
    pub fn pfn_bias(&self) -> u64 {
        self.mem.pfn_bias()
    }

    /// Returns true if the PFNs are contiguous.
    ///
    /// TODO: Fallback allocations are here for now, but we should eventually
    /// allow the caller to require these. See `DmaClient::allocate_dma_buffer`.
    pub fn contiguous_pfns(&self) -> bool {
        for (curr, next) in self.pfns().iter().zip(self.pfns().iter().skip(1)) {
            if *curr + 1 != *next {
                return false;
            }
        }

        true
    }

    /// Gets the buffer as an atomic slice.
    pub fn as_slice(&self) -> &[AtomicU8] {
        // SAFETY: the underlying memory is valid for the lifetime of `mem`.
        unsafe { std::slice::from_raw_parts(self.base.cast(), self.len) }
    }

    /// Reads from the buffer into `data`.
    pub fn read_at(&self, offset: usize, data: &mut [u8]) {
        self.as_slice()[offset..][..data.len()].atomic_read(data);
    }

    /// Reads an object from the buffer at `offset`.
    pub fn read_obj<T: FromBytes + Immutable + KnownLayout>(&self, offset: usize) -> T {
        self.as_slice()[offset..][..size_of::<T>()].atomic_read_obj()
    }

    /// Writes into the buffer from `data`.
    pub fn write_at(&self, offset: usize, data: &[u8]) {
        self.as_slice()[offset..][..data.len()].atomic_write(data);
    }

    /// Writes an object into the buffer at `offset`.
    pub fn write_obj<T: IntoBytes + Immutable + KnownLayout>(&self, offset: usize, data: &T) {
        self.as_slice()[offset..][..size_of::<T>()].atomic_write_obj(data);
    }

    /// Returns the offset of the beginning of the buffer in the first page
    /// returned by [`Self::pfns`].
    pub fn offset_in_page(&self) -> u32 {
        self.base as u32 % PAGE_SIZE as u32
    }
}
