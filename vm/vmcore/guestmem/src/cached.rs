// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Experimental cached access to bounded guest-memory ranges.
//!
//! A backing must explicitly opt in to direct access. Otherwise operations use
//! ordinary fallible [`GuestMemory`] access. Sparse backings report range
//! readiness separately from the lifetime of their VA reservation.
//!
//! One scope can access several caches without entering RCU for every operation:
//!
//! ```
//! use guestmem::cached::with_access;
//! use guestmem::GuestMemory;
//!
//! let mem = GuestMemory::allocate(4096);
//! let mut cache = mem.cache_range(0, 4096)?;
//! with_access(|scope| {
//!     let access = cache.access(scope);
//!     access.write_plain(8, &42u64)?;
//!     assert_eq!(access.read_plain::<u64>(8)?, 42);
//!     Ok::<_, guestmem::GuestMemoryError>(())
//! })?;
//! # Ok::<_, guestmem::GuestMemoryError>(())
//! ```

use crate::AccessType;
use crate::GuestMemory;
use crate::GuestMemoryBackingError;
use crate::GuestMemoryError;
use crate::GuestMemoryErrorKind;
use crate::GuestMemoryOperation;
use crate::OutOfRange;
use crate::SendPtrU8;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Which directions of a range are ready for direct cached access.
#[derive(Debug, Copy, Clone)]
pub struct CachePermissions {
    pub read: bool,
    pub write: bool,
}

impl CachePermissions {
    /// Direct access is unavailable; use ordinary GuestMemory operations.
    pub const NONE: Self = Self {
        read: false,
        write: false,
    };

    /// Both directions are directly accessible, subject to access bitmaps.
    pub const READ_WRITE: Self = Self {
        read: true,
        write: true,
    };
}

/// Coarse invalidation shared by every alias of a cacheable backing.
#[derive(Debug)]
pub struct CacheInvalidation {
    generation: AtomicU64,
}

const READ: u64 = 1;
const WRITE: u64 = 2;
const GENERATION_STEP: u64 = 4;

impl Default for CacheInvalidation {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(GENERATION_STEP),
        }
    }
}

impl CacheInvalidation {
    /// Publishes an invalidation without waiting for readers.
    ///
    /// The backing must close access admission before this call, then await
    /// [`crate::rcu()`]'s grace period outside reader-needed locks/services
    /// before retiring the old backing. Already acquired views can continue
    /// using their pointers until their enclosing scope ends.
    ///
    /// Call again after preparing newly accessible backing or permissions.
    pub fn invalidate(&self) {
        self.generation
            .fetch_update(Ordering::Release, Ordering::Relaxed, |generation| {
                generation.checked_add(GENERATION_STEP)
            })
            .expect("guest memory cache generation exhausted");
    }
}

/// Evidence that the current thread is inside a cached-access RCU scope.
///
/// The scope and views borrowed from it cannot cross threads or escape:
///
/// ```compile_fail
/// use guestmem::{cached::with_access, GuestMemory};
/// let mem = GuestMemory::allocate(4096);
/// let mut cache = mem.cache_range(0, 4096).unwrap();
/// let escaped = with_access(|scope| cache.access(scope));
/// escaped.read_plain::<u64>(0).unwrap();
/// ```
///
/// ```compile_fail
/// use guestmem::{cached::with_access, GuestMemory};
/// let mem = GuestMemory::allocate(4096);
/// let mut cache = mem.cache_range(0, 4096).unwrap();
/// with_access(|scope| {
///     let access = cache.access(scope);
///     std::thread::scope(|threads| {
///         threads.spawn(|| access.read_plain::<u64>(0));
///     });
/// });
/// ```
pub struct AccessScope {
    _thread_bound: PhantomData<*mut ()>,
}

/// Runs bounded synchronous work in one guest-memory RCU critical section.
///
/// Ordinary GuestMemory calls and cache fallbacks are allowed, but must not
/// wait directly or indirectly for this scope's grace period. Do not await,
/// wait for readiness, or run an unbounded queue-draining loop here.
#[inline]
pub fn with_access<R>(f: impl FnOnce(&AccessScope) -> R) -> R {
    crate::rcu().run(|| {
        f(&AccessScope {
            _thread_bound: PhantomData,
        })
    })
}

/// A persistent bounded range with a lazily replaced translation.
///
/// This object does not hold a read-side critical section or lock guest pages.
/// Each [`access`](Self::access) validates its generation once for the resulting
/// view. A range must fit one GuestMemory region.
#[derive(Debug, inspect::Inspect)]
#[inspect(skip)]
pub struct CachedRange {
    memory: GuestMemory,
    len: usize,
    translation: CachedTranslation,
}

#[derive(Debug)]
struct CachedTranslation {
    gpa: u64,
    invalidation: Option<Arc<CacheInvalidation>>,
    mapping: Option<SendPtrU8>,
    state: u64,
}

/// A persistent logical byte range assembled from guest page numbers.
///
/// Page order and duplicates are preserved. No pages are pinned or faulted in.
/// Direct read/write admission is all-or-nothing for the complete list.
/// Refresh requires exclusive ownership; admitted views borrow the entire cache.
#[derive(Debug)]
pub struct CachedPages {
    description: CachedPagesDescription,
    direct: Option<CachedPageTable>,
}

#[derive(Debug, Clone)]
struct CachedPagesDescription {
    memory: GuestMemory,
    gpns: Box<[u64]>,
    len: usize,
}

#[derive(Debug, Clone)]
struct CachedPageTable {
    pointers: Vec<SendPtrU8>,
    domains: Box<[CachedPagesDomain]>,
    ready: bool,
}

#[derive(Debug, Clone)]
struct CachedPagesDomain {
    invalidation: Arc<CacheInvalidation>,
    generation: u64,
}

/// A page cache that requires direct-cache support from every backing.
///
/// Admission prepares unavailable translations or returns an error before
/// scoped work starts. It never substitutes ordinary memory operations.
/// Clones own independent mutable snapshots and share invalidation domains.
#[derive(Debug, Clone)]
pub struct RequiredCachedPages {
    description: CachedPagesDescription,
    table: CachedPageTable,
}

/// A whole page table admitted for read/write access in one RCU scope.
pub struct DirectPagesAccess<'a> {
    description: &'a CachedPagesDescription,
    pointers: &'a [SendPtrU8],
    _scope: PhantomData<&'a AccessScope>,
}

/// An ordinary, non-cached page-list view.
pub struct UncachedPagesAccess<'a> {
    description: &'a CachedPagesDescription,
    _scope: PhantomData<&'a AccessScope>,
}

/// One admitted page, with GPA resolution deferred to error reporting.
///
/// Admission establishes page-aligned GPAs and mappings, so scalar alignment
/// checks only need to examine the offset.
pub struct DirectAccess<'a> {
    description: &'a CachedPagesDescription,
    index: usize,
    mapping: NonNull<u8>,
    _scope: PhantomData<&'a AccessScope>,
}

/// Operations shared by admitted and ordinary page accessors.
pub trait PageAccess {
    fn read_plain<T: FromBytes + Immutable + KnownLayout>(
        &self,
        offset: u64,
    ) -> Result<T, GuestMemoryError>;
    fn write_plain<T: IntoBytes + Immutable + KnownLayout>(
        &self,
        offset: u64,
        value: &T,
    ) -> Result<(), GuestMemoryError>;
    fn read_at(&self, offset: u64, data: &mut [u8]) -> Result<(), GuestMemoryError>;
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), GuestMemoryError>;
    fn load_u32(&self, offset: u64, order: Ordering) -> Result<u32, GuestMemoryError>;
    fn store_u32(&self, offset: u64, value: u32, order: Ordering) -> Result<(), GuestMemoryError>;
}

/// A page list whose direct/ordinary implementation was selected by its caller.
pub trait PagesAccess {
    type Page: PageAccess;
    /// Indexes the already-selected view. `None` means out of bounds, not a
    /// per-page cache miss or permission failure.
    fn page(&self, index: usize) -> Option<Self::Page>;
    fn read_at(&self, offset: usize, data: &mut [u8]) -> Result<(), GuestMemoryError>;
    fn write_at(&self, offset: usize, data: &[u8]) -> Result<(), GuestMemoryError>;
}

impl GuestMemory {
    /// Creates a page cache with no ordinary-access fallback.
    pub fn cache_pages_required(
        &self,
        gpns: &[u64],
    ) -> Result<RequiredCachedPages, GuestMemoryError> {
        let pages = self.cache_pages(gpns)?;
        let table = pages.direct.ok_or_else(|| {
            let gpa = gpns
                .iter()
                .map(|&gpn| gpn * crate::PAGE_SIZE64)
                .find(|&gpa| self.inner.imp.cache_invalidation(gpa).is_none())
                .unwrap_or(0);
            self.wrap_err(
                Some((gpa, crate::PAGE_SIZE64)),
                GuestMemoryOperation::Subrange,
                GuestMemoryBackingError::other(
                    gpa,
                    std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "backing does not support required cached access",
                    ),
                ),
            )
        })?;
        Ok(RequiredCachedPages {
            description: pages.description,
            table,
        })
    }

    /// Caches an aligned, possibly scattered list of guest pages.
    pub fn cache_pages(&self, gpns: &[u64]) -> Result<CachedPages, GuestMemoryError> {
        self.with_op(None, GuestMemoryOperation::Subrange, || {
            let len = gpns.len().checked_mul(crate::PAGE_SIZE).ok_or_else(|| {
                GuestMemoryBackingError::new(GuestMemoryErrorKind::OutOfRange, 0, OutOfRange)
            })?;
            for &gpn in gpns {
                let gpa = crate::gpn_to_gpa(gpn).map_err(GuestMemoryBackingError::gpn)?;
                self.inner.region(gpa, crate::PAGE_SIZE as u64)?;
            }
            let direct = self.cache_page_table(gpns);
            if direct.is_some() && !gpns.is_empty() && !cfg!(miri) {
                trycopy::initialize_try_copy();
            }
            Ok(CachedPages {
                description: CachedPagesDescription {
                    memory: self.clone(),
                    gpns: gpns.into(),
                    len,
                },
                direct,
            })
        })
    }

    /// Creates a translation cache without preemptively locking or faulting in
    /// pages. Backings without direct-cache support retain ordinary access.
    pub fn cache_range(&self, gpa: u64, len: usize) -> Result<CachedRange, GuestMemoryError> {
        self.with_op(
            Some((gpa, len as u64)),
            GuestMemoryOperation::Subrange,
            || {
                let translation = self.cache_translation(gpa, len)?;
                if translation.invalidation.is_some() && !cfg!(miri) {
                    trycopy::initialize_try_copy();
                }
                Ok(CachedRange {
                    memory: self.clone(),
                    len,
                    translation,
                })
            },
        )
    }

    fn cache_translation(
        &self,
        gpa: u64,
        len: usize,
    ) -> Result<CachedTranslation, GuestMemoryBackingError> {
        self.inner.region(gpa, len as u64)?;
        Ok(CachedTranslation {
            gpa,
            invalidation: self.inner.imp.cache_invalidation(gpa),
            mapping: None,
            state: 0,
        })
    }

    fn cache_page_table(&self, gpns: &[u64]) -> Option<CachedPageTable> {
        let mut domains = Vec::new();
        let mut indices = HashMap::new();
        for &gpn in gpns {
            let invalidation = self
                .inner
                .imp
                .cache_invalidation(gpn * crate::PAGE_SIZE64)?;
            indices
                .entry(Arc::as_ptr(&invalidation))
                .or_insert_with(|| {
                    domains.push(CachedPagesDomain {
                        invalidation,
                        generation: 0,
                    });
                });
        }
        Some(CachedPageTable {
            pointers: Vec::with_capacity(gpns.len()),
            domains: domains.into_boxed_slice(),
            ready: gpns.is_empty(),
        })
    }
}

impl RequiredCachedPages {
    /// Number of bytes represented by the page list.
    pub fn len(&self) -> usize {
        self.description.len
    }

    /// Whether the page list is empty.
    pub fn is_empty(&self) -> bool {
        self.description.gpns.is_empty()
    }

    /// Admits all pages for read/write access before any scoped work starts.
    #[inline]
    pub fn access<'a>(
        &'a mut self,
        _scope: &'a AccessScope,
    ) -> Result<DirectPagesAccess<'a>, GuestMemoryError> {
        if !self.table.is_current() || !self.table.ready {
            self.refresh()?;
        }
        Ok(DirectPagesAccess {
            description: &self.description,
            pointers: &self.table.pointers,
            _scope: PhantomData,
        })
    }

    #[cold]
    fn refresh(&mut self) -> Result<(), GuestMemoryError> {
        self.table.refresh(&self.description);
        if self.table.is_current() && self.table.ready {
            return Ok(());
        }
        let memory = &self.description.memory;
        memory.with_op(None, GuestMemoryOperation::Probe, || {
            for &gpn in &self.description.gpns {
                memory
                    .inner
                    .imp
                    .prepare_cached_access(gpn * crate::PAGE_SIZE64, crate::PAGE_SIZE)?;
            }
            self.table.refresh(&self.description);
            if self.table.is_current() && self.table.ready {
                Ok(())
            } else {
                Err(GuestMemoryBackingError::other(
                    self.description
                        .gpns
                        .get(self.table.pointers.len())
                        .or_else(|| self.description.gpns.first())
                        .copied()
                        .unwrap_or(0)
                        * crate::PAGE_SIZE64,
                    std::io::Error::other(
                        "required cached pages are not ready for read/write access",
                    ),
                ))
            }
        })
    }
}

impl CachedPages {
    /// Inline state and translation-table bytes, excluding shared backing allocations.
    pub fn metadata_size(&self) -> usize {
        size_of::<Self>()
            + size_of_val(self.description.gpns.as_ref())
            + self.direct.as_ref().map_or(0, |direct| {
                direct.pointers.capacity() * size_of::<SendPtrU8>()
                    + size_of_val(direct.domains.as_ref())
            })
    }

    /// Number of bytes represented by the page list.
    pub fn len(&self) -> usize {
        self.description.len
    }

    /// Whether the page list is empty.
    pub fn is_empty(&self) -> bool {
        self.description.gpns.is_empty()
    }

    /// Admits the entire table for read/write access, or selects no direct cache.
    ///
    /// Warm admission checks each distinct backing generation once. Page
    /// readiness and permissions are scanned only after a generation change.
    ///
    /// ```compile_fail
    /// use guestmem::{GuestMemory, cached::{with_access, PagesAccess, PageAccess}};
    /// let memory = GuestMemory::allocate(4096);
    /// let mut cache = memory.cache_pages(&[0]).unwrap();
    /// let escaped = with_access(|scope| cache.try_access(scope).unwrap());
    /// escaped.page(0).unwrap().read_plain::<u32>(0).unwrap();
    /// ```
    ///
    /// ```compile_fail
    /// use guestmem::{GuestMemory, cached::{with_access, PagesAccess, PageAccess}};
    /// let memory = GuestMemory::allocate(4096);
    /// let mut cache = memory.cache_pages(&[0]).unwrap();
    /// with_access(|scope| {
    ///     let first = cache.try_access(scope).unwrap();
    ///     let second = cache.try_access(scope);
    ///     first.page(0).unwrap().read_plain::<u32>(0).unwrap();
    /// });
    /// ```
    ///
    /// ```compile_fail
    /// use guestmem::{GuestMemory, cached::{with_access, PagesAccess, PageAccess}};
    /// let memory = GuestMemory::allocate(4096);
    /// let mut cache = memory.cache_pages(&[0]).unwrap();
    /// with_access(|scope| {
    ///     let pages = cache.try_access(scope).unwrap();
    ///     std::thread::scope(|threads| {
    ///         threads.spawn(|| pages.page(0).unwrap().read_plain::<u32>(0).unwrap());
    ///     });
    /// });
    /// ```
    #[inline]
    pub fn try_access<'a>(&'a mut self, _scope: &'a AccessScope) -> Option<DirectPagesAccess<'a>> {
        let table = self.direct.as_mut()?;
        if !table.is_current() {
            table.refresh(&self.description);
            if !table.is_current() {
                return None;
            }
        }
        if !table.ready {
            return None;
        }
        Some(DirectPagesAccess {
            description: &self.description,
            pointers: &table.pointers,
            _scope: PhantomData,
        })
    }

    /// Uses ordinary GuestMemory for every page, without consulting page caches.
    pub fn uncached<'a>(&'a self, _scope: &'a AccessScope) -> UncachedPagesAccess<'a> {
        UncachedPagesAccess {
            description: &self.description,
            _scope: PhantomData,
        }
    }
}

impl CachedPageTable {
    #[cold]
    fn refresh(&mut self, description: &CachedPagesDescription) {
        self.ready = false;
        self.pointers.clear();
        // Capture every dependency before preparing any pointer. A concurrent
        // invalidation must never stamp old pointers with its newer generation.
        for domain in &mut self.domains {
            domain.generation = domain.invalidation.generation.load(Ordering::Acquire);
        }
        for &gpn in &description.gpns {
            let mut translation = CachedTranslation {
                gpa: gpn * crate::PAGE_SIZE64,
                invalidation: None,
                mapping: None,
                state: 0,
            };
            if translation.refresh(&description.memory, crate::PAGE_SIZE, 0) != (READ | WRITE) {
                return;
            }
            let pointer = translation.mapping.expect("admitted translation");
            if !(pointer.0.as_ptr() as usize).is_multiple_of(crate::PAGE_SIZE) {
                return;
            }
            self.pointers.push(pointer);
        }
        self.ready = true;
    }

    #[inline]
    fn is_current(&self) -> bool {
        self.domains.iter().all(|domain| {
            domain.generation == domain.invalidation.generation.load(Ordering::Acquire)
        })
    }
}

impl<'a> PagesAccess for DirectPagesAccess<'a> {
    type Page = DirectAccess<'a>;

    #[inline]
    fn page(&self, index: usize) -> Option<Self::Page> {
        self.pointers.get(index).map(|pointer| DirectAccess {
            description: self.description,
            index,
            mapping: pointer.0,
            _scope: self._scope,
        })
    }

    #[inline]
    fn read_at(&self, offset: usize, data: &mut [u8]) -> Result<(), GuestMemoryError> {
        read_pages(self.description, self, offset, data)
    }

    #[inline]
    fn write_at(&self, offset: usize, data: &[u8]) -> Result<(), GuestMemoryError> {
        write_pages(self.description, self, offset, data)
    }
}

impl<'a> PagesAccess for UncachedPagesAccess<'a> {
    type Page = CachedAccess<'a>;

    fn page(&self, index: usize) -> Option<Self::Page> {
        self.description.gpns.get(index).map(|gpn| CachedAccess {
            memory: &self.description.memory,
            gpa: gpn * crate::PAGE_SIZE64,
            len: crate::PAGE_SIZE,
            read_mapping: None,
            write_mapping: None,
            _scope: self._scope,
        })
    }

    fn read_at(&self, offset: usize, data: &mut [u8]) -> Result<(), GuestMemoryError> {
        read_pages(self.description, self, offset, data)
    }

    fn write_at(&self, offset: usize, data: &[u8]) -> Result<(), GuestMemoryError> {
        write_pages(self.description, self, offset, data)
    }
}

#[inline]
fn read_pages(
    cache: &CachedPagesDescription,
    pages: &impl PagesAccess,
    mut offset: usize,
    mut data: &mut [u8],
) -> Result<(), GuestMemoryError> {
    check_pages(cache, offset, data.len(), GuestMemoryOperation::Read)?;
    while !data.is_empty() {
        let page_offset = offset % crate::PAGE_SIZE;
        let len = data.len().min(crate::PAGE_SIZE - page_offset);
        let (this, next) = data.split_at_mut(len);
        pages
            .page(offset / crate::PAGE_SIZE)
            .unwrap()
            .read_at(page_offset as u64, this)?;
        offset += len;
        data = next;
    }
    Ok(())
}

#[inline]
fn write_pages(
    cache: &CachedPagesDescription,
    pages: &impl PagesAccess,
    mut offset: usize,
    mut data: &[u8],
) -> Result<(), GuestMemoryError> {
    check_pages(cache, offset, data.len(), GuestMemoryOperation::Write)?;
    while !data.is_empty() {
        let page_offset = offset % crate::PAGE_SIZE;
        let len = data.len().min(crate::PAGE_SIZE - page_offset);
        let (this, next) = data.split_at(len);
        pages
            .page(offset / crate::PAGE_SIZE)
            .unwrap()
            .write_at(page_offset as u64, this)?;
        offset += len;
        data = next;
    }
    Ok(())
}

fn check_pages(
    cache: &CachedPagesDescription,
    offset: usize,
    len: usize,
    op: GuestMemoryOperation,
) -> Result<(), GuestMemoryError> {
    if offset > cache.len || len > cache.len - offset {
        Err(cache.memory.wrap_err(
            None,
            op,
            GuestMemoryBackingError::new(GuestMemoryErrorKind::OutOfRange, 0, OutOfRange),
        ))
    } else {
        Ok(())
    }
}

impl PageAccess for DirectAccess<'_> {
    #[inline]
    fn read_plain<T: FromBytes + Immutable + KnownLayout>(
        &self,
        offset: u64,
    ) -> Result<T, GuestMemoryError> {
        let offset = self.range(offset, size_of::<T>(), GuestMemoryOperation::Read)?;
        // SAFETY: the bounded pointer belongs to the admitted page table.
        unsafe { trycopy::try_read_volatile(self.mapping.as_ptr().add(offset).cast::<T>()) }
            .map_err(|fault| {
                Self::fault(
                    self.description,
                    self.index,
                    offset,
                    size_of::<T>(),
                    GuestMemoryOperation::Read,
                    fault,
                )
            })
    }

    #[inline]
    fn write_plain<T: IntoBytes + Immutable + KnownLayout>(
        &self,
        offset: u64,
        value: &T,
    ) -> Result<(), GuestMemoryError> {
        let offset = self.range(offset, size_of::<T>(), GuestMemoryOperation::Write)?;
        // SAFETY: the bounded pointer belongs to the admitted page table.
        unsafe { trycopy::try_write_volatile(self.mapping.as_ptr().add(offset).cast::<T>(), value) }
            .map_err(|fault| {
                Self::fault(
                    self.description,
                    self.index,
                    offset,
                    size_of::<T>(),
                    GuestMemoryOperation::Write,
                    fault,
                )
            })
    }

    #[inline]
    fn read_at(&self, offset: u64, data: &mut [u8]) -> Result<(), GuestMemoryError> {
        let len = data.len();
        let offset = self.range(offset, len, GuestMemoryOperation::Read)?;
        // SAFETY: the admitted page and destination cover the checked length.
        unsafe { trycopy::try_copy(self.mapping.as_ptr().add(offset), data.as_mut_ptr(), len) }
            .map_err(|fault| {
                Self::fault(
                    self.description,
                    self.index,
                    offset,
                    len,
                    GuestMemoryOperation::Read,
                    fault,
                )
            })
    }

    #[inline]
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), GuestMemoryError> {
        let len = data.len();
        let offset = self.range(offset, len, GuestMemoryOperation::Write)?;
        // SAFETY: the admitted page and source cover the checked length.
        unsafe { trycopy::try_copy(data.as_ptr(), self.mapping.as_ptr().add(offset), len) }.map_err(
            |fault| {
                Self::fault(
                    self.description,
                    self.index,
                    offset,
                    len,
                    GuestMemoryOperation::Write,
                    fault,
                )
            },
        )
    }

    #[inline]
    fn load_u32(&self, offset: u64, order: Ordering) -> Result<u32, GuestMemoryError> {
        let offset = self.word(offset, GuestMemoryOperation::Read)?;
        // SAFETY: the admitted page and word offset are naturally aligned.
        unsafe { trycopy::try_load_u32(self.mapping.as_ptr().add(offset).cast(), order) }.map_err(
            |fault| {
                Self::fault(
                    self.description,
                    self.index,
                    offset,
                    4,
                    GuestMemoryOperation::Read,
                    fault,
                )
            },
        )
    }

    #[inline]
    fn store_u32(&self, offset: u64, value: u32, order: Ordering) -> Result<(), GuestMemoryError> {
        let offset = self.word(offset, GuestMemoryOperation::Write)?;
        // SAFETY: the admitted page and word offset are naturally aligned.
        unsafe { trycopy::try_store_u32(self.mapping.as_ptr().add(offset).cast(), value, order) }
            .map_err(|fault| {
                Self::fault(
                    self.description,
                    self.index,
                    offset,
                    4,
                    GuestMemoryOperation::Write,
                    fault,
                )
            })
    }
}

impl DirectAccess<'_> {
    #[inline]
    fn gpa(description: &CachedPagesDescription, index: usize, offset: u64) -> u64 {
        (description.gpns[index] * crate::PAGE_SIZE64).wrapping_add(offset)
    }

    #[inline]
    fn range(
        &self,
        offset: u64,
        len: usize,
        op: GuestMemoryOperation,
    ) -> Result<usize, GuestMemoryError> {
        if offset <= crate::PAGE_SIZE64 && len <= crate::PAGE_SIZE - offset as usize {
            Ok(offset as usize)
        } else {
            Err(Self::range_error(
                self.description,
                self.index,
                offset,
                len,
                op,
            ))
        }
    }

    #[inline]
    fn word(&self, offset: u64, op: GuestMemoryOperation) -> Result<usize, GuestMemoryError> {
        let offset = self.range(offset, 4, op)?;
        if offset.is_multiple_of(4) {
            Ok(offset)
        } else {
            Err(unaligned_word(
                &self.description.memory,
                Self::gpa(self.description, self.index, offset as u64),
                op,
            ))
        }
    }

    #[cold]
    fn range_error(
        description: &CachedPagesDescription,
        index: usize,
        offset: u64,
        len: usize,
        op: GuestMemoryOperation,
    ) -> GuestMemoryError {
        out_of_range(
            &description.memory,
            Self::gpa(description, index, offset),
            len,
            op,
        )
    }

    #[cold]
    fn fault(
        description: &CachedPagesDescription,
        index: usize,
        offset: usize,
        len: usize,
        op: GuestMemoryOperation,
        fault: trycopy::MemoryError,
    ) -> GuestMemoryError {
        cached_fault(
            &description.memory,
            Self::gpa(description, index, offset as u64),
            len,
            op,
            fault,
        )
    }
}

#[inline]
fn check_word_alignment(
    memory: &GuestMemory,
    gpa: u64,
    op: GuestMemoryOperation,
) -> Result<(), GuestMemoryError> {
    if gpa.is_multiple_of(4) {
        Ok(())
    } else {
        Err(unaligned_word(memory, gpa, op))
    }
}

#[cold]
fn unaligned_word(memory: &GuestMemory, gpa: u64, op: GuestMemoryOperation) -> GuestMemoryError {
    memory.wrap_err(
        Some((gpa, 4)),
        op,
        GuestMemoryBackingError::other(
            gpa,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unaligned atomic control word",
            ),
        ),
    )
}

#[cold]
fn out_of_range(
    memory: &GuestMemory,
    gpa: u64,
    len: usize,
    op: GuestMemoryOperation,
) -> GuestMemoryError {
    memory.wrap_err(
        Some((gpa, len as u64)),
        op,
        GuestMemoryBackingError::new(GuestMemoryErrorKind::OutOfRange, gpa, OutOfRange),
    )
}

#[cold]
fn cached_fault(
    memory: &GuestMemory,
    gpa: u64,
    len: usize,
    op: GuestMemoryOperation,
    fault: trycopy::MemoryError,
) -> GuestMemoryError {
    let fault_gpa = gpa.wrapping_add(fault.offset() as u64);
    memory.wrap_err(
        Some((gpa, len as u64)),
        op,
        GuestMemoryBackingError::other(fault_gpa, fault),
    )
}

impl PageAccess for CachedAccess<'_> {
    fn read_plain<T: FromBytes + Immutable + KnownLayout>(
        &self,
        offset: u64,
    ) -> Result<T, GuestMemoryError> {
        CachedAccess::read_plain(self, offset)
    }
    fn write_plain<T: IntoBytes + Immutable + KnownLayout>(
        &self,
        offset: u64,
        value: &T,
    ) -> Result<(), GuestMemoryError> {
        CachedAccess::write_plain(self, offset, value)
    }
    fn read_at(&self, offset: u64, data: &mut [u8]) -> Result<(), GuestMemoryError> {
        CachedAccess::read_at(self, offset, data)
    }
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), GuestMemoryError> {
        CachedAccess::write_at(self, offset, data)
    }
    fn load_u32(&self, offset: u64, order: Ordering) -> Result<u32, GuestMemoryError> {
        CachedAccess::load_u32(self, offset, order)
    }
    fn store_u32(&self, offset: u64, value: u32, order: Ordering) -> Result<(), GuestMemoryError> {
        CachedAccess::store_u32(self, offset, value, order)
    }
}

impl CachedRange {
    /// Whether the backing opted in to direct access preparation.
    ///
    /// This does not imply the range is currently accessible. Callers retaining
    /// legacy subrange optimizations can select them when this returns false.
    pub fn supports_direct_access(&self) -> bool {
        self.translation.invalidation.is_some()
    }

    /// Acquires a view valid until the enclosing RCU scope ends.
    ///
    /// Permission checks are repeated only when the backing generation changes.
    /// Inaccessible or unsupported direct mappings select the fallible path.
    #[inline]
    pub fn access<'a>(&'a mut self, scope: &'a AccessScope) -> CachedAccess<'a> {
        self.translation.access(&self.memory, self.len, scope)
    }

    /// Ordinary access for read-only diagnostics, without refreshing the cache.
    pub fn uncached<'a>(&'a self, _scope: &'a AccessScope) -> CachedAccess<'a> {
        CachedAccess {
            memory: &self.memory,
            gpa: self.translation.gpa,
            len: self.len,
            read_mapping: None,
            write_mapping: None,
            _scope: PhantomData,
        }
    }
}

impl CachedTranslation {
    #[inline]
    fn access<'a>(
        &mut self,
        memory: &'a GuestMemory,
        len: usize,
        _scope: &'a AccessScope,
    ) -> CachedAccess<'a> {
        let state = if let Some(invalidation) = &self.invalidation {
            let generation = invalidation.generation.load(Ordering::Acquire);
            let state = self.state;
            if state & !(READ | WRITE) != generation {
                self.refresh(memory, len, generation)
            } else {
                state
            }
        } else {
            0
        };
        CachedAccess {
            memory,
            gpa: self.gpa,
            len,
            read_mapping: self.mapping.filter(|_| state & READ != 0).map(|ptr| ptr.0),
            write_mapping: self.mapping.filter(|_| state & WRITE != 0).map(|ptr| ptr.0),
            _scope: PhantomData,
        }
    }

    #[cold]
    fn refresh(&mut self, memory: &GuestMemory, len: usize, generation: u64) -> u64 {
        // The region layout is immutable; construction already validated it.
        let (region, region_offset, _) = memory
            .inner
            .region(self.gpa, len as u64)
            .expect("cached range was validated");
        let permits = |access| {
            // SAFETY: cache_range validated bounds; the backing keeps bitmap
            // storage alive and this is called only inside the RCU scope.
            unsafe {
                region
                    .check_access(access, region_offset, len as u64)
                    .is_ok()
            }
        };
        let ready = memory.inner.imp.cached_access(self.gpa, len);
        self.mapping = memory
            .inner
            .imp
            .cached_mapping(self.gpa, len)
            .map(SendPtrU8);
        let mut state = generation;
        if self.mapping.is_some() {
            if ready.read && permits(AccessType::Read) {
                state |= READ;
            }
            if ready.write && permits(AccessType::Write) {
                state |= WRITE;
            }
        }
        self.state = state;
        state
    }
}

/// A scoped range view with fallible direct/slow selection per operation.
///
/// Typed and bulk copies retain trycopy's atomicity, unaligned handling, and
/// fault containment. Admitted-pointer faults return errors without retrying
/// ordinary access; unavailable translations retain ordinary fault handling.
/// Ordered control-word accesses use aligned scalar trycopy with explicit fences.
pub struct CachedAccess<'a> {
    memory: &'a GuestMemory,
    gpa: u64,
    len: usize,
    read_mapping: Option<NonNull<u8>>,
    write_mapping: Option<NonNull<u8>>,
    _scope: PhantomData<&'a AccessScope>,
}

impl CachedAccess<'_> {
    #[inline]
    fn word(
        &self,
        offset: u64,
        op: GuestMemoryOperation,
    ) -> Result<(u64, usize), GuestMemoryError> {
        let (gpa, offset) = self.range(offset, 4, op)?;
        check_word_alignment(self.memory, gpa, op)?;
        Ok((gpa, offset))
    }

    /// Loads an aligned control word with the specified memory ordering.
    ///
    /// Copy-only backings without a mapping return an error: a byte-copy
    /// fallback is not sufficient to implement atomic control-word semantics.
    #[inline]
    pub fn load_u32(&self, offset: u64, order: Ordering) -> Result<u32, GuestMemoryError> {
        assert!(matches!(
            order,
            Ordering::Relaxed | Ordering::Acquire | Ordering::SeqCst
        ));
        let (gpa, offset) = self.word(offset, GuestMemoryOperation::Read)?;
        let read = |pointer: *mut u8| {
            // SAFETY: cached and ordinary paths supply a reserved, bounded word.
            unsafe { trycopy::try_load_u32(pointer.cast::<u32>(), order) }
        };
        if let Some(mapping) = self.read_mapping {
            let pointer = mapping.as_ptr().wrapping_add(offset);
            if pointer.cast::<u32>().is_aligned() {
                let result = read(pointer);
                return result.map_err(|fault| {
                    cached_fault(self.memory, gpa, 4, GuestMemoryOperation::Read, fault)
                });
            }
        }
        self.ordered_fallback(gpa, AccessType::Read, read)
    }

    /// Stores an aligned control word with the specified memory ordering.
    ///
    /// Copy-only backings without a mapping return an error.
    #[inline]
    pub fn store_u32(
        &self,
        offset: u64,
        value: u32,
        order: Ordering,
    ) -> Result<(), GuestMemoryError> {
        assert!(matches!(
            order,
            Ordering::Relaxed | Ordering::Release | Ordering::SeqCst
        ));
        let (gpa, offset) = self.word(offset, GuestMemoryOperation::Write)?;
        let write = |pointer: *mut u8| {
            // SAFETY: cached and ordinary paths supply a reserved, bounded word.
            unsafe { trycopy::try_store_u32(pointer.cast::<u32>(), value, order) }
        };
        if let Some(mapping) = self.write_mapping {
            let pointer = mapping.as_ptr().wrapping_add(offset);
            if pointer.cast::<u32>().is_aligned() {
                let result = write(pointer);
                return result.map_err(|fault| {
                    cached_fault(self.memory, gpa, 4, GuestMemoryOperation::Write, fault)
                });
            }
        }
        self.ordered_fallback(gpa, AccessType::Write, write)
    }

    #[cold]
    fn ordered_fallback<T>(
        &self,
        gpa: u64,
        access: AccessType,
        mut f: impl FnMut(*mut u8) -> Result<T, trycopy::MemoryError>,
    ) -> Result<T, GuestMemoryError> {
        let op = match access {
            AccessType::Read => GuestMemoryOperation::Read,
            AccessType::Write => GuestMemoryOperation::Write,
        };
        self.memory.with_op(Some((gpa, 4)), op, || {
            self.memory
                .run_on_mapping(
                    access,
                    gpa,
                    4,
                    (),
                    |(), pointer| {
                        if !pointer.cast::<u32>().is_aligned() {
                            return Ok(Err(GuestMemoryBackingError::other(
                                gpa,
                                std::io::Error::new(
                                    std::io::ErrorKind::Unsupported,
                                    "ordered control access requires an aligned mapping",
                                ),
                            )));
                        }
                        f(pointer).map(Ok)
                    },
                    |()| {
                        Err(GuestMemoryBackingError::other(
                            gpa,
                            std::io::Error::new(
                                std::io::ErrorKind::Unsupported,
                                "ordered control access requires mapped atomic memory",
                            ),
                        ))
                    },
                )
                .and_then(|result| result)
        })
    }

    #[inline]
    fn range(
        &self,
        offset: u64,
        len: usize,
        op: GuestMemoryOperation,
    ) -> Result<(u64, usize), GuestMemoryError> {
        let gpa = self.gpa.wrapping_add(offset);
        if offset <= self.len as u64 && len <= self.len - offset as usize {
            Ok((gpa, offset as usize))
        } else {
            Err(out_of_range(self.memory, gpa, len, op))
        }
    }
    /// Reads an object, with the same atomicity guarantees as GuestMemory.
    #[inline]
    pub fn read_plain<T: FromBytes + Immutable + KnownLayout>(
        &self,
        offset: u64,
    ) -> Result<T, GuestMemoryError> {
        let (gpa, offset) = self.range(offset, size_of::<T>(), GuestMemoryOperation::Read)?;
        if let Some(mapping) = self.read_mapping {
            // SAFETY: the range is checked and the scoped pointer was admitted.
            unsafe { trycopy::try_read_volatile(mapping.as_ptr().add(offset).cast::<T>()) }.map_err(
                |fault| {
                    cached_fault(
                        self.memory,
                        gpa,
                        size_of::<T>(),
                        GuestMemoryOperation::Read,
                        fault,
                    )
                },
            )
        } else {
            self.memory.read_plain(gpa)
        }
    }

    /// Writes an object, with the same atomicity guarantees as GuestMemory.
    #[inline]
    pub fn write_plain<T: IntoBytes + Immutable + KnownLayout>(
        &self,
        offset: u64,
        value: &T,
    ) -> Result<(), GuestMemoryError> {
        let (gpa, offset) = self.range(offset, size_of::<T>(), GuestMemoryOperation::Write)?;
        if let Some(mapping) = self.write_mapping {
            // SAFETY: the range is checked and the scoped pointer was admitted.
            unsafe { trycopy::try_write_volatile(mapping.as_ptr().add(offset).cast::<T>(), value) }
                .map_err(|fault| {
                    cached_fault(
                        self.memory,
                        gpa,
                        size_of::<T>(),
                        GuestMemoryOperation::Write,
                        fault,
                    )
                })
        } else {
            self.memory.write_plain(gpa, value)
        }
    }

    /// Copies bytes from the cached range.
    #[inline]
    pub fn read_at(&self, offset: u64, dest: &mut [u8]) -> Result<(), GuestMemoryError> {
        let len = dest.len();
        let (gpa, offset) = self.range(offset, len, GuestMemoryOperation::Read)?;
        if let Some(mapping) = self.read_mapping {
            // SAFETY: both buffers cover dest.len() bytes.
            unsafe { trycopy::try_copy(mapping.as_ptr().add(offset), dest.as_mut_ptr(), len) }
                .map_err(|fault| {
                    cached_fault(self.memory, gpa, len, GuestMemoryOperation::Read, fault)
                })
        } else {
            self.memory.read_at(gpa, dest)
        }
    }

    /// Copies bytes into the cached range.
    #[inline]
    pub fn write_at(&self, offset: u64, src: &[u8]) -> Result<(), GuestMemoryError> {
        let len = src.len();
        let (gpa, offset) = self.range(offset, len, GuestMemoryOperation::Write)?;
        if let Some(mapping) = self.write_mapping {
            // SAFETY: both buffers cover src.len() bytes.
            unsafe { trycopy::try_copy(src.as_ptr(), mapping.as_ptr().add(offset), len) }.map_err(
                |fault| cached_fault(self.memory, gpa, len, GuestMemoryOperation::Write, fault),
            )
        } else {
            self.memory.write_at(gpa, src)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AlignedHeapMemory;
    use crate::GuestMemoryAccess;
    use std::io;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use test_with_tracing::test;

    struct TrackedPages {
        memory: AlignedHeapMemory,
        denied: AtomicUsize,
        checks: AtomicUsize,
        preparations: AtomicUsize,
        make_ready: AtomicBool,
    }

    impl TrackedPages {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                memory: AlignedHeapMemory::new(2 * crate::PAGE_SIZE),
                denied: AtomicUsize::new(usize::MAX),
                checks: AtomicUsize::new(0),
                preparations: AtomicUsize::new(0),
                make_ready: AtomicBool::new(false),
            })
        }

        fn invalidate(&self) {
            self.memory.cache_invalidation().unwrap().invalidate();
        }
    }

    // SAFETY: the aligned allocation stays mapped through all scoped uses.
    // Readiness changes only close cache admission, never retire the allocation.
    unsafe impl GuestMemoryAccess for TrackedPages {
        fn mapping(&self) -> Option<NonNull<u8>> {
            self.memory.mapping()
        }
        fn max_address(&self) -> u64 {
            self.memory.max_address()
        }
        fn cache_invalidation(&self) -> Option<Arc<CacheInvalidation>> {
            self.memory.cache_invalidation()
        }
        fn cached_access(&self, address: u64, _: usize) -> CachePermissions {
            self.checks.fetch_add(1, Ordering::Relaxed);
            if address as usize / crate::PAGE_SIZE == self.denied.load(Ordering::Relaxed) {
                CachePermissions::NONE
            } else {
                CachePermissions::READ_WRITE
            }
        }

        fn prepare_cached_access(
            &self,
            address: u64,
            len: usize,
        ) -> Result<(), GuestMemoryBackingError> {
            assert!(address + len as u64 <= self.max_address());
            self.preparations.fetch_add(1, Ordering::Relaxed);
            if self.make_ready.swap(false, Ordering::Relaxed) {
                self.denied.store(usize::MAX, Ordering::Relaxed);
                self.invalidate();
            }
            Ok(())
        }
    }

    #[test]
    fn required_cache_prepares_before_work_and_never_uses_ordinary_access() {
        let backing = TrackedPages::new();
        backing.denied.store(1, Ordering::Relaxed);
        let memory = GuestMemory::new_multi_region(
            "required",
            4 * crate::PAGE_SIZE64,
            vec![None, Some(backing.clone())],
        )
        .unwrap();
        let gpa = 5 * crate::PAGE_SIZE64;
        memory.write_plain(gpa, &123u64).unwrap();
        let mut cache = memory.cache_pages_required(&[4, 5]).unwrap();
        let mut ran = false;
        let result = with_access(|scope| {
            let access = cache.access(scope)?;
            ran = true;
            access.page(1).unwrap().write_plain(0, &456u64)
        });
        assert!(result.is_err());
        assert!(!ran);
        assert_eq!(memory.read_plain::<u64>(gpa).unwrap(), 123);

        backing.make_ready.store(true, Ordering::Relaxed);
        with_access(|scope| {
            let access = cache.access(scope).unwrap();
            assert_eq!(access.page(1).unwrap().read_plain::<u64>(0).unwrap(), 123);
        });
        let checks = backing.checks.load(Ordering::Relaxed);
        let preparations = backing.preparations.load(Ordering::Relaxed);
        with_access(|scope| {
            cache
                .access(scope)
                .unwrap()
                .page(1)
                .unwrap()
                .write_plain(0, &456u64)
                .unwrap();
        });
        assert_eq!(backing.checks.load(Ordering::Relaxed), checks);
        assert_eq!(backing.preparations.load(Ordering::Relaxed), preparations);
        assert_eq!(memory.read_plain::<u64>(gpa).unwrap(), 456);
    }

    #[test]
    fn required_cache_rejects_lockable_backing_without_cache_support() {
        struct Lockable(AlignedHeapMemory);
        // SAFETY: the owned allocation remains mapped for the backing's lifetime.
        unsafe impl GuestMemoryAccess for Lockable {
            fn mapping(&self) -> Option<NonNull<u8>> {
                self.0.mapping()
            }
            fn max_address(&self) -> u64 {
                self.0.max_address()
            }
        }
        let memory = GuestMemory::new(
            "lockable",
            Lockable(AlignedHeapMemory::new(crate::PAGE_SIZE)),
        );
        assert!(memory.supports_user_lock());
        assert!(memory.cache_pages_required(&[0]).is_err());
        let mut optional = memory.cache_pages(&[0]).unwrap();
        with_access(|scope| {
            assert!(optional.try_access(scope).is_none());
            optional
                .uncached(scope)
                .page(0)
                .unwrap()
                .write_plain(0, &42u64)
                .unwrap();
        });
        assert_eq!(memory.read_plain::<u64>(0).unwrap(), 42);
    }

    struct MovingMemory {
        memories: [GuestMemory; 2],
        pointers: [SendPtrU8; 2],
        active: AtomicUsize,
        move_during_prepare: AtomicBool,
        keep_moving: AtomicBool,
        invalidation: Arc<CacheInvalidation>,
    }

    #[test]
    fn required_cache_and_cloned_snapshot_follow_moving_translations() {
        let backing = MovingMemory::new();
        backing.memories[0].write_plain(0, &123u64).unwrap();
        backing.memories[1].write_plain(0, &456u64).unwrap();
        let memory = GuestMemory::new("required-moving", backing.clone());
        let mut cache = memory.cache_pages_required(&[0]).unwrap();
        with_access(|scope| {
            assert_eq!(
                cache
                    .access(scope)
                    .unwrap()
                    .page(0)
                    .unwrap()
                    .read_plain::<u64>(0)
                    .unwrap(),
                123
            );
        });
        let mut cloned = cache.clone();
        backing.switch(1);
        for cache in [&mut cache, &mut cloned] {
            with_access(|scope| {
                assert_eq!(
                    cache
                        .access(scope)
                        .unwrap()
                        .page(0)
                        .unwrap()
                        .read_plain::<u64>(0)
                        .unwrap(),
                    456
                );
            });
        }
    }

    #[test]
    fn required_cache_reports_raced_preparation_without_running_work() {
        let backing = MovingMemory::new();
        backing.keep_moving.store(true, Ordering::Relaxed);
        let memory = GuestMemory::new("required-race", backing);
        let mut cache = memory.cache_pages_required(&[0]).unwrap();
        let mut ran = false;
        let result = with_access(|scope| {
            let access = cache.access(scope)?;
            ran = true;
            access.page(0).unwrap().write_plain(0, &123u64)
        });
        assert!(result.is_err());
        assert!(!ran);
    }

    impl MovingMemory {
        fn new() -> Arc<Self> {
            let heaps = [0; 2].map(|_| Arc::new(AlignedHeapMemory::new(3 * crate::PAGE_SIZE)));
            let pointers = heaps
                .each_ref()
                .map(|heap| SendPtrU8(heap.mapping().unwrap()));
            Arc::new(Self {
                memories: heaps.map(|heap| GuestMemory::new("moving-allocation", heap)),
                pointers,
                active: AtomicUsize::new(0),
                move_during_prepare: AtomicBool::new(false),
                keep_moving: AtomicBool::new(false),
                invalidation: Arc::new(CacheInvalidation::default()),
            })
        }

        fn switch(&self, active: usize) {
            self.active.store(active, Ordering::Release);
            self.invalidation.invalidate();
        }
    }

    // SAFETY: both allocations remain accessible for the backing's lifetime;
    // translation changes invalidate all aliases before returning.
    unsafe impl GuestMemoryAccess for MovingMemory {
        fn mapping(&self) -> Option<NonNull<u8>> {
            None
        }

        fn max_address(&self) -> u64 {
            3 * crate::PAGE_SIZE64
        }

        fn cache_invalidation(&self) -> Option<Arc<CacheInvalidation>> {
            Some(self.invalidation.clone())
        }

        fn cached_mapping(&self, address: u64, len: usize) -> Option<NonNull<u8>> {
            if address.checked_add(len as u64)? > self.max_address() {
                return None;
            }
            let active = self.active.load(Ordering::Acquire);
            // SAFETY: bounds were checked and both allocations remain owned.
            let pointer = unsafe { self.pointers[active].0.add(address as usize) };
            if self.move_during_prepare.swap(false, Ordering::Relaxed)
                || self.keep_moving.load(Ordering::Relaxed)
            {
                self.switch(1 - active);
            }
            Some(pointer)
        }

        unsafe fn read_fallback(
            &self,
            address: u64,
            dest: *mut u8,
            len: usize,
        ) -> Result<(), GuestMemoryBackingError> {
            // SAFETY: the caller supplies the destination for this read.
            let dest = unsafe { std::slice::from_raw_parts_mut(dest, len) };
            self.memories[self.active.load(Ordering::Acquire)]
                .read_at(address, dest)
                .map_err(|error| GuestMemoryBackingError::other(address, error))
        }

        unsafe fn write_fallback(
            &self,
            address: u64,
            src: *const u8,
            len: usize,
        ) -> Result<(), GuestMemoryBackingError> {
            // SAFETY: the caller supplies initialized source bytes.
            let src = unsafe { std::slice::from_raw_parts(src, len) };
            self.memories[self.active.load(Ordering::Acquire)]
                .write_at(address, src)
                .map_err(|error| GuestMemoryBackingError::other(address, error))
        }
    }

    #[test]
    fn invalidation_replaces_range_and_page_pointers_without_a_linear_mapping() {
        let backing = MovingMemory::new();
        for (index, memory) in backing.memories.iter().enumerate() {
            for page in 0..3 {
                memory
                    .write_plain(
                        page * crate::PAGE_SIZE64,
                        &(10 * index as u32 + page as u32),
                    )
                    .unwrap();
            }
        }
        let memory = GuestMemory::new("moving", backing.clone());
        let mut range = memory.cache_range(0, 64).unwrap();
        let mut pages = memory.cache_pages(&[2, 0, 1, 0]).unwrap();
        assert!(range.supports_direct_access());
        with_access(|scope| {
            let old_range = range.access(scope);
            let old_pages = pages.try_access(scope).unwrap();
            assert_eq!(old_range.load_u32(0, Ordering::Acquire).unwrap(), 0);
            assert_eq!(old_pages.page(0).unwrap().read_plain::<u32>(0).unwrap(), 2);
            backing.switch(1);
            assert_eq!(old_range.read_plain::<u32>(0).unwrap(), 0);
            // A previously untouched page still uses the admitted old table.
            assert_eq!(old_pages.page(2).unwrap().read_plain::<u32>(0).unwrap(), 1);
        });
        with_access(|scope| {
            let new_range = range.access(scope);
            let new_pages = pages.try_access(scope).unwrap();
            assert_eq!(new_range.read_plain::<u32>(0).unwrap(), 10);
            assert_eq!(new_pages.page(0).unwrap().read_plain::<u32>(0).unwrap(), 12);
            new_pages
                .page(1)
                .unwrap()
                .store_u32(0, 99, Ordering::Release)
                .unwrap();
            assert_eq!(
                new_pages
                    .page(3)
                    .unwrap()
                    .load_u32(0, Ordering::Acquire)
                    .unwrap(),
                99
            );
        });
        assert_eq!(backing.memories[0].read_plain::<u32>(0).unwrap(), 0);
        assert_eq!(backing.memories[1].read_plain::<u32>(0).unwrap(), 99);
    }

    #[test]
    fn invalidation_during_preparation_cannot_stamp_old_pointers_with_a_new_generation() {
        let backing = MovingMemory::new();
        backing.memories[0].write_plain(0, &11u32).unwrap();
        backing.memories[1].write_plain(0, &22u32).unwrap();
        let memory = GuestMemory::new("moving-during-prepare", backing.clone());
        let mut range = memory.cache_range(0, 64).unwrap();
        backing.move_during_prepare.store(true, Ordering::Relaxed);
        with_access(|scope| {
            assert_eq!(range.access(scope).read_plain::<u32>(0).unwrap(), 11);
            assert_eq!(range.access(scope).read_plain::<u32>(0).unwrap(), 22);
        });
        backing.switch(0);
        let mut pages = memory.cache_pages(&[0, 1]).unwrap();
        backing.move_during_prepare.store(true, Ordering::Relaxed);
        with_access(|scope| {
            assert!(pages.try_access(scope).is_none());
            assert_eq!(
                pages
                    .try_access(scope)
                    .unwrap()
                    .page(0)
                    .unwrap()
                    .read_plain::<u32>(0)
                    .unwrap(),
                22
            );
        });
    }

    #[test]
    fn aggregate_admission_covers_every_page_and_backing() {
        let first = TrackedPages::new();
        let second = TrackedPages::new();
        let memory = GuestMemory::new_multi_region(
            "aggregate",
            4 * crate::PAGE_SIZE as u64,
            vec![Some(first.clone()), Some(second.clone())],
        )
        .unwrap();
        let mut cache = memory.cache_pages(&[0, 4, 1, 5, 0]).unwrap();
        let mut retained = memory.cache_pages(&[0, 4, 1, 5, 0]).unwrap();
        with_access(|scope| {
            let admitted = cache.try_access(scope).unwrap();
            admitted.page(0).unwrap().write_plain(0, &123u32).unwrap();
            let initial = [
                first.checks.load(Ordering::Relaxed),
                second.checks.load(Ordering::Relaxed),
            ];
            for _ in 0..8 {
                let admitted = cache.try_access(scope).unwrap();
                assert_eq!(admitted.page(4).unwrap().read_plain::<u32>(0).unwrap(), 123);
            }
            assert_eq!(
                initial,
                [
                    first.checks.load(Ordering::Relaxed),
                    second.checks.load(Ordering::Relaxed)
                ]
            );

            // An unavailable page that this copy never touches still rejects
            // the entire direct table.
            let admitted = retained.try_access(scope).unwrap();
            second.denied.store(1, Ordering::Relaxed);
            second.invalidate();
            assert!(cache.try_access(scope).is_none());
            let checks = second.checks.load(Ordering::Relaxed);
            let mut bytes = [0; 4];
            cache.uncached(scope).read_at(0, &mut bytes).unwrap();
            assert_eq!(u32::from_ne_bytes(bytes), 123);
            assert_eq!(second.checks.load(Ordering::Relaxed), checks);
            // Another owner's admitted view retains its old pointer table.
            assert_eq!(admitted.page(4).unwrap().read_plain::<u32>(0).unwrap(), 123);

            second.denied.store(usize::MAX, Ordering::Relaxed);
            assert!(cache.try_access(scope).is_none());
            second.invalidate();
            assert!(cache.try_access(scope).is_some());
            first.denied.store(0, Ordering::Relaxed);
            first.invalidate();
            assert!(cache.try_access(scope).is_none());
        });
    }

    fn check_cache_only_fault_containment(pages: bool) {
        struct CacheOnly {
            mapping: sparse_mmap::SparseMapping,
            invalidation: Arc<CacheInvalidation>,
        }

        // SAFETY: the committed allocation remains owned and accessible for
        // every admitted view. Only the test's accessor is replaced below.
        unsafe impl GuestMemoryAccess for CacheOnly {
            fn mapping(&self) -> Option<NonNull<u8>> {
                None
            }
            fn max_address(&self) -> u64 {
                self.mapping.len() as u64
            }
            fn cache_invalidation(&self) -> Option<Arc<CacheInvalidation>> {
                Some(self.invalidation.clone())
            }
            fn cached_mapping(&self, address: u64, len: usize) -> Option<NonNull<u8>> {
                if address.checked_add(len as u64)? > self.max_address() {
                    return None;
                }
                NonNull::new(
                    self.mapping
                        .as_ptr()
                        .cast::<u8>()
                        .wrapping_add(address as usize),
                )
            }
        }

        let mapping = sparse_mmap::SparseMapping::new(crate::PAGE_SIZE).unwrap();
        mapping.alloc(0, crate::PAGE_SIZE).unwrap();
        let memory = GuestMemory::new(
            "cache-only",
            CacheOnly {
                mapping,
                invalidation: Arc::new(CacheInvalidation::default()),
            },
        );
        let faulting = sparse_mmap::SparseMapping::new(crate::PAGE_SIZE).unwrap();
        if pages {
            let mut cache = memory.cache_pages(&[0]).unwrap();
            with_access(|scope| {
                let mut page = cache.try_access(scope).unwrap().page(0).unwrap();
                assert_eq!(page.read_plain::<u32>(0).unwrap(), 0);
                page.mapping = NonNull::new(faulting.as_ptr().cast()).unwrap();
                assert_eq!(
                    page.read_plain::<u32>(0).unwrap_err().kind(),
                    GuestMemoryErrorKind::Other
                );
            });
        } else {
            let mut cache = memory.cache_range(0, 4).unwrap();
            with_access(|scope| {
                let mut access = cache.access(scope);
                assert_eq!(access.read_plain::<u32>(0).unwrap(), 0);
                access.read_mapping = NonNull::new(faulting.as_ptr().cast());
                assert_eq!(
                    access.read_plain::<u32>(0).unwrap_err().kind(),
                    GuestMemoryErrorKind::Other
                );
            });
        }
    }

    #[test]
    fn range_cache_only_mapping_installs_fault_containment() {
        check_cache_only_fault_containment(false);
    }

    #[test]
    fn page_cache_only_mapping_installs_fault_containment() {
        check_cache_only_fault_containment(true);
    }

    #[test]
    fn admitted_pointer_faults_return_errors_without_ordinary_retry() {
        fn assert_fault(error: GuestMemoryError, gpa: u64) {
            assert_eq!(error.kind(), GuestMemoryErrorKind::Other);
            assert!(error.to_string().contains(&format!("{gpa:#x}")));
            let mut source: &(dyn std::error::Error + 'static) = &error;
            loop {
                if source.is::<trycopy::MemoryError>() {
                    break;
                }
                source = source.source().expect("trycopy fault source");
            }
        }

        fn exercise(page: &impl PageAccess, gpa: u64) {
            for (load, store) in [
                (Ordering::Relaxed, Ordering::Relaxed),
                (Ordering::Acquire, Ordering::Release),
                (Ordering::SeqCst, Ordering::SeqCst),
            ] {
                assert_fault(page.store_u32(4, 99, store).unwrap_err(), gpa + 4);
                assert_fault(page.load_u32(4, load).unwrap_err(), gpa + 4);
            }
            assert_fault(page.read_plain::<u64>(16).unwrap_err(), gpa + 16);
            assert_fault(page.write_plain(16, &789u64).unwrap_err(), gpa + 16);
            assert_fault(page.write_at(32, &[1, 2, 3, 4, 5]).unwrap_err(), gpa + 32);
            let mut bytes = [0xff; 5];
            assert_fault(page.read_at(32, &mut bytes).unwrap_err(), gpa + 32);
            assert_eq!(bytes, [0xff; 5]);
        }

        let memory = GuestMemory::allocate(4 * crate::PAGE_SIZE);
        memory
            .write_plain(2 * crate::PAGE_SIZE64 + 4, &123u32)
            .unwrap();
        memory
            .write_plain(3 * crate::PAGE_SIZE64 + 4, &456u32)
            .unwrap();
        let range_before = memory
            .read_plain::<[u8; crate::PAGE_SIZE]>(2 * crate::PAGE_SIZE64)
            .unwrap();
        let page_before = memory
            .read_plain::<[u8; crate::PAGE_SIZE]>(3 * crate::PAGE_SIZE64)
            .unwrap();
        let mut cache = memory
            .cache_range(2 * crate::PAGE_SIZE64, crate::PAGE_SIZE)
            .unwrap();
        let faulting = sparse_mmap::SparseMapping::new(crate::PAGE_SIZE).unwrap();
        let pointer = NonNull::new(faulting.as_ptr().cast::<u8>()).unwrap();

        with_access(|scope| {
            let mut access = cache.access(scope);
            // Inject a reserved but unbacked candidate without changing the
            // real backing's permission or invalidation contract.
            access.read_mapping = Some(pointer);
            access.write_mapping = Some(pointer);
            exercise(&access, 2 * crate::PAGE_SIZE64);
        });
        let mut pages = memory.cache_pages(&[3, 1, 3]).unwrap();
        with_access(|scope| {
            let admitted = pages.try_access(scope).unwrap();
            let mut page = admitted.page(2).unwrap();
            assert!(page.load_u32(1, Ordering::Acquire).is_err());
            assert!(page.store_u32(1, 0, Ordering::Release).is_err());
            assert_eq!(
                page.read_plain::<u64>(crate::PAGE_SIZE64 - 4)
                    .unwrap_err()
                    .kind(),
                GuestMemoryErrorKind::OutOfRange,
            );
            page.mapping = pointer;
            exercise(&page, 3 * crate::PAGE_SIZE64);
        });
        assert_eq!(
            memory
                .read_plain::<[u8; crate::PAGE_SIZE]>(2 * crate::PAGE_SIZE64)
                .unwrap(),
            range_before
        );
        assert_eq!(
            memory
                .read_plain::<[u8; crate::PAGE_SIZE]>(3 * crate::PAGE_SIZE64)
                .unwrap(),
            page_before
        );
    }

    #[test]
    fn scattered_pages_cross_boundaries_and_preserve_aliases() {
        let memory = GuestMemory::allocate(4 * crate::PAGE_SIZE);
        let mut cache = memory.cache_pages(&[3, 0, 2, 0]).unwrap();
        let len = cache.len();
        with_access(|scope| {
            let access = cache.try_access(scope).unwrap();
            access
                .write_at(crate::PAGE_SIZE - 3, &[1, 2, 3, 4, 5, 6])
                .unwrap();
            let mut bytes = [0; 6];
            access.read_at(crate::PAGE_SIZE - 3, &mut bytes).unwrap();
            assert_eq!(bytes, [1, 2, 3, 4, 5, 6]);
            access
                .read_at(3 * crate::PAGE_SIZE, &mut bytes[..3])
                .unwrap();
            assert_eq!(&bytes[..3], &[4, 5, 6]);
            assert!(access.write_at(len - 1, &[9, 9]).is_err());
            assert!(access.read_at(usize::MAX, &mut bytes).is_err());
            access.write_at(len, &[]).unwrap();
            access.read_at(len, &mut []).unwrap();
        });
        assert_eq!(
            memory
                .read_plain::<[u8; 3]>(4 * crate::PAGE_SIZE as u64 - 3)
                .unwrap(),
            [1, 2, 3]
        );
        assert_eq!(memory.read_plain::<[u8; 3]>(0).unwrap(), [4, 5, 6]);
        assert_eq!(
            memory
                .read_plain::<u8>(crate::PAGE_SIZE as u64 - 1)
                .unwrap(),
            0
        );
        assert!(memory.cache_pages(&[u64::MAX]).is_err());
        assert!(memory.cache_pages(&[4]).is_err());
        let mut empty = memory.cache_pages(&[]).unwrap();
        with_access(|scope| {
            empty
                .try_access(scope)
                .unwrap()
                .read_at(0, &mut [])
                .unwrap()
        });
    }

    #[test]
    fn bounded_unaligned_and_cross_page_access() {
        let memory = GuestMemory::allocate(3 * crate::PAGE_SIZE);
        let start = crate::PAGE_SIZE as u64 - 3;
        let mut cache = memory.cache_range(start, 32).unwrap();
        with_access(|scope| {
            let access = cache.access(scope);
            access.write_plain(1, &0x1122334455667788u64).unwrap();
            assert_eq!(access.read_plain::<u64>(1).unwrap(), 0x1122334455667788);
            access.write_at(15, &[3; 17]).unwrap();
            let mut data = [0; 17];
            access.read_at(15, &mut data).unwrap();
            assert_eq!(data, [3; 17]);
            assert_eq!(
                access.write_plain(31, &0xffffu16).unwrap_err().kind(),
                GuestMemoryErrorKind::OutOfRange
            );
            assert!(access.read_plain::<u8>(32).is_err());
            assert!(access.read_plain::<u8>(u64::MAX).is_err());
            assert!(access.write_at(31, &[9; 2]).is_err());
            access.read_at(32, &mut []).unwrap();
            access.write_at(32, &[]).unwrap();
        });
        assert_eq!(memory.read_plain::<u8>(start + 31).unwrap(), 3);
        assert_eq!(memory.read_plain::<u8>(start + 32).unwrap(), 0);
        assert!(memory.cache_range(u64::MAX, 1).is_err());
        assert!(memory.cache_range(0, usize::MAX).is_err());
    }

    struct FallbackMemory {
        memory: GuestMemory,
        fail_write: AtomicBool,
    }

    // SAFETY: only fallback operations are exposed, with the caller's buffers.
    unsafe impl GuestMemoryAccess for FallbackMemory {
        fn mapping(&self) -> Option<NonNull<u8>> {
            None
        }

        fn max_address(&self) -> u64 {
            4096
        }

        unsafe fn read_fallback(
            &self,
            address: u64,
            dest: *mut u8,
            len: usize,
        ) -> Result<(), GuestMemoryBackingError> {
            // SAFETY: the tests supply unaliased destination buffers.
            let dest = unsafe { std::slice::from_raw_parts_mut(dest, len) };
            self.memory
                .read_at(address, dest)
                .map_err(|err| GuestMemoryBackingError::other(address, err))
        }

        unsafe fn write_fallback(
            &self,
            address: u64,
            src: *const u8,
            len: usize,
        ) -> Result<(), GuestMemoryBackingError> {
            if self.fail_write.load(Ordering::Relaxed) {
                return Err(GuestMemoryBackingError::other(
                    address,
                    io::Error::other("injected cached fallback failure"),
                ));
            }
            // SAFETY: the tests supply initialized source buffers.
            let src = unsafe { std::slice::from_raw_parts(src, len) };
            self.memory
                .write_at(address, src)
                .map_err(|err| GuestMemoryBackingError::other(address, err))
        }
    }

    #[test]
    fn pointerless_fallback_and_nested_ordinary_access() {
        let backing = Arc::new(FallbackMemory {
            memory: GuestMemory::allocate(4096),
            fail_write: AtomicBool::new(false),
        });
        let memory = GuestMemory::new("fallback", backing.clone());
        assert!(!memory.supports_user_lock());
        let mut cache = memory.cache_range(128, 16).unwrap();
        with_access(|scope| {
            let access = cache.access(scope);
            access.write_plain(0, &1234u32).unwrap();
            assert_eq!(access.read_plain::<u32>(0).unwrap(), 1234);
            access.write_at(4, &[4, 5, 6]).unwrap();
            let mut data = [0; 3];
            access.read_at(4, &mut data).unwrap();
            assert_eq!(data, [4, 5, 6]);
            assert_eq!(memory.read_plain::<u32>(128).unwrap(), 1234);
            backing.fail_write.store(true, Ordering::Relaxed);
            let error = access.write_plain(0, &99u32).unwrap_err();
            let mut source: &dyn std::error::Error = &error;
            while let Some(next) = source.source() {
                source = next;
            }
            assert_eq!(source.to_string(), "injected cached fallback failure");
            assert!(access.write_at(0, &[9]).is_err());
            assert_eq!(access.read_plain::<u32>(0).unwrap(), 1234);
        });
        backing.fail_write.store(false, Ordering::Relaxed);
        let mut pages = memory.cache_pages(&[0, 0]).unwrap();
        with_access(|scope| {
            assert!(pages.try_access(scope).is_none());
            let access = pages.uncached(scope);
            access
                .write_at(crate::PAGE_SIZE - 2, &[1, 2, 3, 4])
                .unwrap();
            let page = access.page(1).unwrap();
            assert!(page.store_u32(8, 456, Ordering::SeqCst).is_err());
            assert!(page.load_u32(8, Ordering::Acquire).is_err());
            assert!(page.load_u32(1, Ordering::Relaxed).is_err());
            assert!(page.store_u32(1, 0, Ordering::Relaxed).is_err());
            backing.fail_write.store(true, Ordering::Relaxed);
            assert!(page.store_u32(8, 0, Ordering::Release).is_err());
            assert!(page.load_u32(8, Ordering::SeqCst).is_err());
        });
    }

    #[test]
    fn multi_region_and_subrange_fallback() {
        let size = crate::PAGE_SIZE as u64;
        let memory = GuestMemory::new_multi_region(
            "regions",
            4 * size,
            vec![
                Some(AlignedHeapMemory::new(size as usize)),
                None,
                Some(AlignedHeapMemory::new(size as usize)),
            ],
        )
        .unwrap();
        let mut first = memory.cache_range(64, 16).unwrap();
        let mut last = memory.cache_range(8 * size + 64, 16).unwrap();
        let subrange = memory.subrange(8 * size + 64, 16, false).unwrap();
        let mut subrange_cache = subrange.cache_range(0, 16).unwrap();
        with_access(|scope| {
            let first = first.access(scope);
            let last = last.access(scope);
            let subrange = subrange_cache.access(scope);
            first.write_plain(0, &11u64).unwrap();
            last.write_plain(0, &22u64).unwrap();
            assert_eq!(first.read_plain::<u64>(0).unwrap(), 11);
            assert_eq!(subrange.read_plain::<u64>(0).unwrap(), 22);
            subrange.store_u32(8, 123, Ordering::SeqCst).unwrap();
            assert_eq!(subrange.load_u32(8, Ordering::Acquire).unwrap(), 123);
        });
        assert!(memory.cache_range(4 * size, 1).is_err());
        assert!(memory.cache_range(size - 1, 2).is_err());
    }

    #[test]
    fn unwind_exits_scope() {
        let memory = GuestMemory::allocate(4096);
        let mut cache = memory.cache_range(0, 8).unwrap();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_access(|scope| {
                cache.access(scope).write_plain(0, &7u64).unwrap();
                panic!("scope unwind");
            });
        }));
        assert!(unwind.is_err());
        crate::rcu().synchronize_blocking();
        with_access(|scope| assert_eq!(cache.access(scope).read_plain::<u64>(0).unwrap(), 7));
    }

    #[cfg(feature = "bitmap")]
    mod bitmap {
        use super::*;
        use crate::BitmapInfo;
        use sparse_mmap::SparseMapping;
        use std::future::Future;
        use std::sync::atomic::AtomicU8;
        use std::sync::mpsc;
        use std::task::Context;
        use std::task::Poll;
        use std::task::Waker;
        use std::time::Duration;
        use test_with_tracing::test;

        struct RevocableMemory {
            mapping: SparseMapping,
            read: AtomicU8,
            write: AtomicU8,
            invalidation: Arc<CacheInvalidation>,
        }

        impl RevocableMemory {
            fn new() -> Arc<Self> {
                let mapping = SparseMapping::new(2 * crate::PAGE_SIZE).unwrap();
                mapping.alloc(0, mapping.len()).unwrap();
                Arc::new(Self {
                    mapping,
                    read: AtomicU8::new(3),
                    write: AtomicU8::new(3),
                    invalidation: Arc::new(CacheInvalidation::default()),
                })
            }
        }

        // SAFETY: admitted pages remain mapped through the grace period;
        // permissions are closed before invalidation and retirement.
        unsafe impl GuestMemoryAccess for RevocableMemory {
            fn mapping(&self) -> Option<NonNull<u8>> {
                NonNull::new(self.mapping.as_ptr().cast())
            }

            fn max_address(&self) -> u64 {
                self.mapping.len() as u64
            }

            fn cache_invalidation(&self) -> Option<Arc<CacheInvalidation>> {
                Some(self.invalidation.clone())
            }

            fn supports_user_lock(&self) -> bool {
                false
            }

            fn supports_kernel_lock(&self) -> bool {
                false
            }

            fn access_bitmap(&self) -> Option<BitmapInfo> {
                Some(BitmapInfo {
                    read_bitmap: NonNull::from(&self.read).cast(),
                    write_bitmap: NonNull::from(&self.write).cast(),
                    bit_offset: 0,
                })
            }
        }

        #[test]
        fn held_reader_delays_async_revocation_and_regrant_refreshes() {
            let backing = RevocableMemory::new();
            let memory = GuestMemory::new("revocable", backing.clone());
            assert!(!memory.supports_user_lock());
            let mut cache = memory.cache_range(0, 64).unwrap();
            let mut alias_cache = memory.clone().cache_range(0, 64).unwrap();
            let mut pages = memory.cache_pages(&[0, 1]).unwrap();
            let (closed_send, closed_recv) = mpsc::channel();
            let (exited_send, exited_recv) = mpsc::channel();
            std::thread::scope(|threads| {
                with_access(|scope| {
                    let access = cache.access(scope);
                    let admitted = pages.try_access(scope).unwrap();
                    let page = admitted.page(0).unwrap();
                    access.store_u32(0, 123, Ordering::SeqCst).unwrap();
                    let backing = backing.clone();
                    threads.spawn(move || {
                        backing.read.store(0, Ordering::Relaxed);
                        backing.write.store(0, Ordering::Relaxed);
                        backing.invalidation.invalidate();
                        let mut drain = std::pin::pin!(crate::rcu().synchronize(async |_| {
                            std::future::pending::<()>().await;
                        }));
                        let mut cx = Context::from_waker(Waker::noop());
                        assert!(matches!(drain.as_mut().poll(&mut cx), Poll::Pending));
                        closed_send.send(()).unwrap();
                        exited_recv.recv_timeout(Duration::from_secs(5)).unwrap();
                        assert!(matches!(drain.as_mut().poll(&mut cx), Poll::Ready(())));
                        backing.mapping.unmap(0, backing.mapping.len()).unwrap();
                    });
                    closed_recv.recv_timeout(Duration::from_secs(5)).unwrap();
                    // This view was admitted before closure and must not consult
                    // the newly cleared bitmap on each operation.
                    access.store_u32(0, 456, Ordering::Release).unwrap();
                    assert_eq!(access.load_u32(0, Ordering::Acquire).unwrap(), 456);
                    assert_eq!(page.load_u32(0, Ordering::Acquire).unwrap(), 456);
                    assert_eq!(
                        admitted
                            .page(1)
                            .unwrap()
                            .load_u32(0, Ordering::Acquire)
                            .unwrap(),
                        0
                    );
                    assert!(pages.try_access(scope).is_none());
                    let denied = alias_cache.access(scope);
                    assert!(denied.read_plain::<u64>(0).is_err());
                    assert!(denied.write_plain(0, &0u64).is_err());
                    assert!(denied.load_u32(0, Ordering::SeqCst).is_err());
                    assert!(denied.store_u32(0, 0, Ordering::SeqCst).is_err());
                });
                exited_send.send(()).unwrap();
            });
            with_access(|scope| {
                assert!(cache.access(scope).read_plain::<u64>(0).is_err());
                assert!(pages.try_access(scope).is_none());
            });
            backing.mapping.alloc(0, backing.mapping.len()).unwrap();
            backing.mapping.write_at(0, &789u64.to_ne_bytes()).unwrap();
            backing.read.store(3, Ordering::Relaxed);
            backing.write.store(3, Ordering::Relaxed);
            backing.invalidation.invalidate();
            with_access(|scope| {
                assert_eq!(cache.access(scope).read_plain::<u64>(0).unwrap(), 789);
                assert_eq!(
                    pages
                        .try_access(scope)
                        .unwrap()
                        .page(0)
                        .unwrap()
                        .read_plain::<u64>(0)
                        .unwrap(),
                    789
                );
            });
        }

        #[test]
        fn independent_caches_refresh_during_concurrent_permission_changes() {
            let backing = RevocableMemory::new();
            let memory = GuestMemory::new("shared-cache", backing.clone());
            memory.write_plain(0, &42u64).unwrap();
            let mut cache = memory.cache_range(0, 64).unwrap();
            let mut page_cache = memory.cache_pages(&[1, 0]).unwrap();
            let start = std::sync::Barrier::new(5);
            std::thread::scope(|threads| {
                for _ in 0..4 {
                    let mut cache = memory.cache_range(0, 64).unwrap();
                    let mut page_cache = memory.cache_pages(&[1, 0]).unwrap();
                    let start = &start;
                    threads.spawn(move || {
                        start.wait();
                        for _ in 0..256 {
                            with_access(|scope| {
                                let access = cache.access(scope);
                                assert_eq!(access.read_plain::<u64>(0).unwrap(), 42);
                                let _ = access.write_plain(0, &42u64);
                                if let Some(pages) = page_cache.try_access(scope) {
                                    let page = pages.page(1).unwrap();
                                    assert_eq!(page.read_plain::<u64>(0).unwrap(), 42);
                                    page.write_plain(0, &42u64).unwrap();
                                } else {
                                    assert_eq!(
                                        page_cache
                                            .uncached(scope)
                                            .page(1)
                                            .unwrap()
                                            .read_plain::<u64>(0)
                                            .unwrap(),
                                        42
                                    );
                                }
                            });
                        }
                    });
                }
                start.wait();
                for _ in 0..4 {
                    backing.write.store(0, Ordering::Relaxed);
                    backing.invalidation.invalidate();
                    crate::rcu().synchronize_blocking();
                    with_access(|scope| {
                        assert!(cache.access(scope).write_plain(0, &99u64).is_err());
                        assert!(page_cache.try_access(scope).is_none());
                    });
                    backing.write.store(3, Ordering::Relaxed);
                    backing.invalidation.invalidate();
                }
            });
            with_access(|scope| {
                let access = cache.access(scope);
                access.write_plain(0, &123u64).unwrap();
                assert_eq!(access.read_plain::<u64>(0).unwrap(), 123);
            });
        }

        #[test]
        fn direction_and_cross_page_permissions() {
            let backing = RevocableMemory::new();
            let memory = GuestMemory::new("permissions", backing.clone());
            let mut pages = memory.cache_pages(&[1, 0, 1]).unwrap();
            let mut cache = memory.cache_range(0, 16).unwrap();
            with_access(|scope| cache.access(scope).write_plain(0, &42u64).unwrap());
            backing.write.store(0, Ordering::Relaxed);
            backing.invalidation.invalidate();
            crate::rcu().synchronize_blocking();
            with_access(|scope| {
                let access = cache.access(scope);
                assert_eq!(access.read_plain::<u64>(0).unwrap(), 42);
                assert!(access.write_plain(0, &0u64).is_err());
            });
            backing.write.store(3, Ordering::Relaxed);
            backing.read.store(2, Ordering::Relaxed);
            backing.invalidation.invalidate();
            crate::rcu().synchronize_blocking();
            with_access(|scope| {
                assert!(pages.try_access(scope).is_none());
                let pages = pages.uncached(scope);
                assert!(
                    pages
                        .page(1)
                        .unwrap()
                        .load_u32(0, Ordering::Acquire)
                        .is_err()
                );
                assert!(
                    pages
                        .page(0)
                        .unwrap()
                        .load_u32(0, Ordering::Acquire)
                        .is_ok()
                );
                assert!(
                    pages
                        .page(2)
                        .unwrap()
                        .load_u32(0, Ordering::Acquire)
                        .is_ok()
                );
            });
            with_access(|scope| {
                let access = cache.access(scope);
                assert!(access.read_plain::<u64>(0).is_err());
                access.write_plain(0, &99u64).unwrap();
            });
            backing.read.store(1, Ordering::Relaxed);
            backing.write.store(1, Ordering::Relaxed);
            backing.invalidation.invalidate();
            crate::rcu().synchronize_blocking();
            let mut crossing = memory.cache_range(crate::PAGE_SIZE as u64 - 1, 2).unwrap();
            with_access(|scope| {
                let access = crossing.access(scope);
                access.write_plain(0, &7u8).unwrap();
                assert_eq!(access.read_plain::<u8>(0).unwrap(), 7);
                assert!(access.read_plain::<u8>(1).is_err());
                assert!(access.read_plain::<u16>(0).is_err());
                assert!(access.write_plain(0, &99u16).is_err());
            });
        }
    }
}
