// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows "soft large pages" (Transparent Huge Pages) for the primary VA
//! mapper.
//!
//! THP-eligible, writable guest RAM on the primary mapper is committed/mapped
//! **read-only** (see [`SoftLp::deferred_protect`]); the first *write* to a
//! 2 MB window raises it to read-write and prefetches it, so the OS can back it
//! with a contiguous large page that the hypervisor maps as a single 2 MB SLAT
//! entry instead of the 512 fragmented 4 KB entries that dribbled per-page
//! writes would produce. Both fault paths funnel through the same
//! [`raise_window`](SoftLp::raise_window):
//!
//! - [`SoftLp::resolve`] — a guest access forwarded by the hypervisor; it also
//!   decides the range to hand back so a full window populates the SLAT as one
//!   2 MB entry.
//! - [`SoftLp::on_host_write`] — a host-side write (e.g. the loader) that traps
//!   through the VA mapper's `page_fault`.
//!
//! Funneling through one routine means each window is protected and prefetched
//! exactly once regardless of which side touches it first.
//!
//! `protect` invalidates the partition's SLAT mapping of the affected range
//! (splitting any large page and forcing every page in it to re-fault), so it
//! must run exactly once per window: re-protecting an already-raised window
//! would invalidate all ~512 of its pages, each re-faulting and re-protecting —
//! a self-sustaining fault storm. A lock-free "raised" bitmap ([`FirstFault`])
//! is the fast path, and a per-mapping [`raise_lock`](SoftLp::raise_lock) elects
//! a single protector so concurrent faulters of the same still-read-only window
//! sleep on the lock rather than re-fault.
//!
//! This module is compiled only on Windows; other targets get an uninhabited
//! stub (see the non-Windows `soft_lp` module) so the fault paths need no
//! per-item `cfg`s. Even on Windows, [`SoftLp::new`] returns `None` for
//! non-THP, read-only, or non-primary mappings, so those pay nothing.

use super::super::manager::MemoryPolicy;
use inspect::Inspect;
use inspect_counters::SharedCounter;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use sparse_mmap::SparseMapping;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use thiserror::Error;
use windows_sys::Win32::System::Memory::PAGE_READWRITE;

/// The soft-large-page granularity (2 MB), used for opportunistic large SLAT
/// entries and the per-region raised bitmap.
pub(super) const LARGE_PAGE_SIZE: u64 = 2 * 1024 * 1024;

/// Failure raising a 2 MB window to read-write. Carries the range and the
/// underlying OS error as its [`source`](std::error::Error::source), so the
/// passed-up guest-memory error identifies the failed operation rather than
/// surfacing a bare OS error code.
#[derive(Debug, Error)]
#[error("failed to raise window {0} to read-write")]
pub(super) struct RaiseError(MemoryRange, #[source] std::io::Error);

/// Per-mapping soft-large-page state and logic.
///
/// Constructed (via [`SoftLp::new`]) only for THP-eligible, writable
/// primary-mapper RAM on Windows; stored inline in the VA mapper's per-mapping
/// properties and driven by the guest ([`resolve`](Self::resolve)) and host
/// ([`on_host_write`](Self::on_host_write)) fault paths.
#[derive(Debug)]
pub(super) struct SoftLp {
    /// Per-2 MB-window "raised" bitmap, the lock-free fast path for the raise.
    raised: FirstFault,
    /// Serializes raising a window to read-write within this mapping. Each
    /// mapping is a single VAD, so `protect` already takes that VAD's exclusive
    /// lock in the kernel; this userspace lock therefore adds no serialization
    /// the kernel would not already impose, and lets a concurrent faulter of the
    /// same (still-read-only) window sleep here until the winner's `protect`
    /// lands rather than re-faulting in a tight loop. Only the raise takes it;
    /// the already-raised fast path is lock-free.
    raise_lock: Mutex<()>,
    /// Whether the mapping is committed/mapped read-only at build time (so the
    /// first write faults and raises the window). False for prefetched ranges,
    /// which are populated read-write eagerly.
    deferred_protect: bool,
    stats: SoftLpStats,
}

impl SoftLp {
    /// Creates soft-LP state for a mapping, or `None` if soft large pages do not
    /// apply: a non-Windows host, THP disabled, or a read-only / non-primary
    /// mapping. (Only the **primary** mapper matters — the partition and loader
    /// run against its VA, so it is the only place a large host backing yields a
    /// 2 MB SLAT entry.)
    ///
    /// `prefetch` marks a range populated read-write eagerly at build time: its
    /// windows start already raised and it is not deferred-protected.
    pub(super) fn new(
        range: MemoryRange,
        policy: &MemoryPolicy,
        writable: bool,
        primary: bool,
    ) -> Option<Self> {
        // Soft large pages are THP-only and matter only on the writable primary
        // mapper (the partition and loader run against its VA).
        let applies = policy.transparent_hugepages && writable && primary;
        if !applies {
            return None;
        }
        let regions = large_region_count(range);
        let stats = SoftLpStats::default();
        // Prefetched ranges are populated up front, so mark every window raised
        // and count them as eager promotions; lazy ranges start unraised.
        let raised = if policy.prefetch {
            stats.eager_promotions.add(regions);
            FirstFault::new_all_raised(regions)
        } else {
            FirstFault::new(regions)
        };
        Some(Self {
            raised,
            raise_lock: Mutex::new(()),
            deferred_protect: !policy.prefetch,
            stats,
        })
    }

    /// Whether the mapping should be committed/mapped read-only at build time so
    /// the first write faults and raises the covering window. False for
    /// prefetched (eagerly populated read-write) ranges.
    pub(super) fn deferred_protect(&self) -> bool {
        self.deferred_protect
    }

    /// Resolves a guest fault against this mapping.
    ///
    /// The first *write* to a window raises it to read-write (a read is served
    /// read-only from the zero page, spending a large page only on the written
    /// working set). Returns the range the hypervisor should map: the whole
    /// 2 MB window when a write lands on a full aligned window (so it maps as one
    /// large SLAT entry, whether this fault or an earlier host write raised it),
    /// otherwise the single faulting page. `start`/`end` are the mapping's first
    /// and inclusive-last addresses.
    pub(super) fn resolve(
        &self,
        mapping: &SparseMapping,
        fault: MemoryRange,
        write: bool,
        start: u64,
        end: u64,
    ) -> Result<MemoryRange, RaiseError> {
        let base = fault.start() & !(LARGE_PAGE_SIZE - 1);
        let window_end = base + LARGE_PAGE_SIZE;
        let full_window = fault.end() <= window_end && base >= start && window_end <= end + 1;
        if write {
            let region = base / LARGE_PAGE_SIZE - start / LARGE_PAGE_SIZE;
            // Whole 2 MB window clamped to the mapping; for a full window this is
            // exactly `base..window_end`.
            let prot = MemoryRange::new(base.max(start)..window_end.min(end + 1));
            if self.raise_window(mapping, region, prot, full_window)? && full_window {
                self.stats.guest_promotions.increment();
            }
            if full_window {
                // The window is now read-write across its whole span; map it as
                // one 2 MB SLAT entry.
                return Ok(MemoryRange::new(base..window_end));
            }
        }
        Ok(fault)
    }

    /// Raises the 2 MB window covering a host write fault (e.g. the loader or a
    /// device DMA writing guest RAM through the VA mapping). `start`/`end` are
    /// the mapping's first and inclusive-last addresses.
    pub(super) fn on_host_write(
        &self,
        mapping: &SparseMapping,
        address: u64,
        start: u64,
        end: u64,
    ) -> Result<(), RaiseError> {
        self.stats.deferred_protect_faults.increment();
        let win_base = address & !(LARGE_PAGE_SIZE - 1);
        let full_window = win_base >= start && win_base + LARGE_PAGE_SIZE <= end + 1;
        let region = win_base / LARGE_PAGE_SIZE - start / LARGE_PAGE_SIZE;
        // Whole 2 MB window clamped to the mapping.
        let prot = MemoryRange::new(win_base.max(start)..(win_base + LARGE_PAGE_SIZE).min(end + 1));
        if self.raise_window(mapping, region, prot, full_window)? && full_window {
            self.stats.host_promotions.increment();
        }
        Ok(())
    }

    /// Raises a window to read-write exactly once, serialized by
    /// [`raise_lock`](Self::raise_lock), and — for a full 2 MB window —
    /// prefetches it so the OS can back it with a contiguous large page.
    ///
    /// A lock-free `is_raised` fast path skips the raise for already-read-write
    /// windows; otherwise the lock elects a single protector (the losers sleep
    /// on the lock, not re-fault, and find the window raised on wake). The bit
    /// is set only after a successful `protect`, so a failed protect leaves the
    /// window unraised for a later fault to retry. `prot` must be the whole 2 MB
    /// window clamped to the mapping, never just the faulting page: a later write
    /// to any other page in the window finds it raised and skips the protect, so
    /// every page must already be read-write.
    ///
    /// Returns `Ok(true)` if this call performed the raise, `Ok(false)` if the
    /// window was already raised (read-write on return either way).
    fn raise_window(
        &self,
        mapping: &SparseMapping,
        region: u64,
        prot: MemoryRange,
        full_window: bool,
    ) -> Result<bool, RaiseError> {
        // Fast path: already raised, no lock.
        if self.raised.is_raised(region) {
            return Ok(false);
        }
        {
            let _guard = self.raise_lock.lock();
            // Re-check under the lock: a concurrent winner may have raised it
            // while we waited.
            if self.raised.is_raised(region) {
                return Ok(false);
            }
            mapping
                .protect(prot.start() as usize, prot.len() as usize, PAGE_READWRITE)
                .map_err(|e| RaiseError(prot, e))?;
            self.raised.set_raised(region);
        }
        // Prefetch outside the lock (best-effort): only a full window can be
        // backed by a large page. A failure (pre-Win11, or no large page free)
        // just leaves 4 KB backing.
        if full_window {
            if let Err(err) = mapping.prefetch(prot.start() as usize, prot.len() as usize) {
                self.stats.prefetch_failures.increment();
                tracing::debug!(
                    error = &err as &dyn std::error::Error,
                    gpa = prot.start(),
                    "soft large page prefetch failed"
                );
            }
        }
        Ok(true)
    }
}

impl Inspect for SoftLp {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.stats.inspect(req);
    }
}

/// Per-mapping counters for the soft-large-page (2 MB window) machinery,
/// exposed via `Inspect`.
///
/// Kept per mapping (one backing, and thus one set of counters, per NUMA node)
/// so they scale for large multi-NUMA-node VMs rather than contending on a
/// single global counter.
#[derive(Debug, Default, Inspect)]
struct SoftLpStats {
    /// 2 MB windows raised + prefetched on the host (loader/DMA) fault path.
    host_promotions: SharedCounter,
    /// 2 MB windows raised + prefetched on the guest fault path.
    guest_promotions: SharedCounter,
    /// 2 MB windows pre-populated eagerly at build time (prefetch).
    eager_promotions: SharedCounter,
    /// Best-effort prefetch failures (no free large page, or pre-Win11).
    prefetch_failures: SharedCounter,
    /// Host write faults that trapped into a deferred-protect (read-only)
    /// window (e.g. the loader's first write to a window).
    deferred_protect_faults: SharedCounter,
}

/// Number of aligned 2 MB regions spanned by `range` (by absolute 2 MB region
/// index), used to size a per-range [`FirstFault`] bitmap.
fn large_region_count(range: MemoryRange) -> u64 {
    (range.end() - 1) / LARGE_PAGE_SIZE - range.start() / LARGE_PAGE_SIZE + 1
}

/// Per-2 MB-region "raised" bitmap for a single THP-eligible mapping. A set bit
/// means the region has been raised to read-write (its deferred protect has been
/// applied). The bit is set only *after* the `protect` succeeds and while
/// holding the mapping's `raise_lock`, so a failed raise leaves it clear (and is
/// retried by a later fault); a lock-free reader uses it as the fast path that
/// skips the raise for already-read-write windows.
#[derive(Debug)]
struct FirstFault(Box<[AtomicU64]>);

impl FirstFault {
    /// Allocates a bitmap covering `regions` 2 MB regions, all bits clear (no
    /// window raised yet).
    fn new(regions: u64) -> Self {
        let words = regions.div_ceil(64);
        Self(
            (0..words)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
    }

    /// Allocates a bitmap covering `regions` 2 MB regions with every window
    /// already marked raised. Used for prefetched ranges: they are populated
    /// read-write eagerly at build time, so the lazy `resolve` path never needs
    /// to raise them. Bits past `regions` in the final word are set too, but are
    /// never indexed.
    fn new_all_raised(regions: u64) -> Self {
        let words = regions.div_ceil(64);
        Self(
            (0..words)
                .map(|_| AtomicU64::new(!0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
    }

    /// Returns whether `region` has been raised. Lock-free (`Acquire`), used as
    /// the fast path before taking the raise lock.
    fn is_raised(&self, region: u64) -> bool {
        let word = (region / 64) as usize;
        let bit = 1u64 << (region % 64);
        match self.0.get(word) {
            Some(slot) => slot.load(Ordering::Acquire) & bit != 0,
            None => false,
        }
    }

    /// Marks `region` raised. Called under the mapping's `raise_lock` after the
    /// `protect` succeeds; the `Release` pairs with [`FirstFault::is_raised`]'s
    /// `Acquire` so a fast-path reader that sees the bit also sees the protect.
    fn set_raised(&self, region: u64) {
        let word = (region / 64) as usize;
        let bit = 1u64 << (region % 64);
        if let Some(slot) = self.0.get(word) {
            slot.fetch_or(bit, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FirstFault;
    use super::LARGE_PAGE_SIZE;
    use super::large_region_count;
    use memory_range::MemoryRange;

    /// A window starts unraised and, once `set_raised`, reads back raised.
    #[test]
    fn raise_is_sticky() {
        let ff = FirstFault::new(4);
        // Every region starts unraised.
        for region in 0..4 {
            assert!(!ff.is_raised(region), "region {region} starts unraised");
        }
        // Raising a region sticks.
        for region in 0..4 {
            ff.set_raised(region);
            assert!(ff.is_raised(region), "region {region} raised");
        }
    }

    /// Raises are independent across regions, including across the 64-bit word
    /// boundary of the underlying bitmap.
    #[test]
    fn raises_are_independent() {
        // Enough regions to span more than one 64-bit word.
        let ff = FirstFault::new(130);
        ff.set_raised(0);
        ff.set_raised(64);
        // Raising one region does not raise its neighbors.
        assert!(ff.is_raised(0));
        assert!(!ff.is_raised(1));
        assert!(ff.is_raised(64));
        assert!(!ff.is_raised(63));
        assert!(!ff.is_raised(65));
        assert!(!ff.is_raised(129));
    }

    /// Region indices past the end of the allocated bitmap report unraised
    /// rather than panicking, so a stray query can never index out of bounds.
    #[test]
    fn out_of_range_returns_false() {
        // One region rounds up to a single 64-bit word, so word 1 and beyond are
        // out of range.
        let ff = FirstFault::new(1);
        assert!(!ff.is_raised(64));
        assert!(!ff.is_raised(1_000_000));
        // Setting an out-of-range region is a no-op, not a panic.
        ff.set_raised(1_000_000);
        assert!(!ff.is_raised(1_000_000));
    }

    /// `new_all_raised` starts fully raised, so no in-range window is ever
    /// raised again (used for eagerly prefetched ranges).
    #[test]
    fn new_all_raised_is_fully_raised() {
        let regions = 100;
        let ff = FirstFault::new_all_raised(regions);
        for region in 0..regions {
            assert!(ff.is_raised(region), "region {region} should be raised");
        }
    }

    /// `large_region_count` counts the aligned 2 MB regions a range spans by
    /// absolute region index, so a range that straddles a 2 MB boundary spans
    /// two regions even when it is smaller than 2 MB.
    #[test]
    fn large_region_count_spans() {
        const LP: u64 = LARGE_PAGE_SIZE;
        // A single page at the start of a region spans one region.
        assert_eq!(large_region_count(MemoryRange::new(0..0x1000)), 1);
        // A full aligned region spans one region.
        assert_eq!(large_region_count(MemoryRange::new(0..LP)), 1);
        // A range straddling a 2 MB boundary spans two regions.
        assert_eq!(
            large_region_count(MemoryRange::new(LP - 0x1000..LP + 0x1000)),
            2
        );
        // Two full aligned regions span two regions.
        assert_eq!(large_region_count(MemoryRange::new(0..2 * LP)), 2);
    }
}
