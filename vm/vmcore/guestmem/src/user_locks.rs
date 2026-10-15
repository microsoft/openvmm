// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPU-only page loans and transient mapping/permission changes.

use parking_lot::Mutex;
use std::ops::Range;
use std::sync::Arc;
use thiserror::Error;

/// Failure to reserve a usermode loan or a mapping/permission change.
#[derive(Debug, Error)]
pub enum GuestMemoryUserLockError {
    #[error("invalid or overflowing guest-memory lock range")]
    InvalidRange,
    #[error("guest page at {0:#x} is locked by userspace")]
    UserLocked(u64),
    #[error("guest-memory mapping or permissions are changing")]
    Changing,
    #[error("failed to allocate guest-memory lock metadata")]
    Allocation(#[source] std::collections::TryReserveError),
}

#[derive(Debug)]
struct Change {
    id: u64,
    ranges: Box<[Range<u64>]>,
}

#[derive(Debug, Default)]
struct State {
    loans: Vec<Box<[u64]>>,
    changes: Vec<Change>,
    next_change: u64,
    waiters: Vec<UnlockWaiter>,
}

struct UnlockWaiter {
    ranges: Vec<Range<u64>>,
    callback: Box<dyn FnOnce() + Send + Sync>,
}

impl std::fmt::Debug for UnlockWaiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnlockWaiter")
            .field("ranges", &self.ranges)
            .finish_non_exhaustive()
    }
}

/// Shared by a memory backing and every operation that can revoke its loans.
///
/// Ordinary fault-contained copies do not register here. A loan starts before
/// probing pages and remains registered until its lock handle is dropped.
#[derive(Debug, Default)]
pub struct GuestMemoryUserLocks {
    state: Mutex<State>,
}

fn page_range(gpn: u64) -> Result<Range<u64>, GuestMemoryUserLockError> {
    let start = gpn
        .checked_mul(crate::PAGE_SIZE as u64)
        .ok_or(GuestMemoryUserLockError::InvalidRange)?;
    let end = start
        .checked_add(crate::PAGE_SIZE as u64)
        .ok_or(GuestMemoryUserLockError::InvalidRange)?;
    Ok(start..end)
}

fn overlaps(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

impl GuestMemoryUserLocks {
    /// Reserves pages before a backing exposes their CPU addresses.
    ///
    /// Repeated and overlapping loans are allowed. Unlock must receive the
    /// same ordered GPN list as one successful reservation.
    pub fn lock_gpns(&self, gpns: &[u64]) -> Result<(), GuestMemoryUserLockError> {
        if gpns.is_empty() {
            return Ok(());
        }
        for &gpn in gpns {
            page_range(gpn)?;
        }
        let mut loan = Vec::new();
        loan.try_reserve_exact(gpns.len())
            .map_err(GuestMemoryUserLockError::Allocation)?;
        loan.extend_from_slice(gpns);
        let mut state = self.state.lock();
        for &gpn in gpns {
            let page = page_range(gpn)?;
            if state
                .changes
                .iter()
                .flat_map(|change| &change.ranges)
                .any(|range| overlaps(&page, range))
            {
                return Err(GuestMemoryUserLockError::Changing);
            }
        }
        state
            .loans
            .try_reserve(1)
            .map_err(GuestMemoryUserLockError::Allocation)?;
        state.loans.push(loan.into_boxed_slice());
        Ok(())
    }

    /// Releases one successful reservation.
    pub fn unlock_gpns(&self, gpns: &[u64]) {
        if gpns.is_empty() {
            return;
        }
        let mut state = self.state.lock();
        let index = state
            .loans
            .iter()
            .position(|loan| loan.as_ref() == gpns)
            .expect("guest-memory loan was not reserved");
        state.loans.swap_remove(index);
        let mut ready = Vec::new();
        let mut index = 0;
        while index < state.waiters.len() {
            if state.loans.iter().flat_map(|loan| loan.iter()).any(|&gpn| {
                let page = page_range(gpn).expect("reserved page range is valid");
                state.waiters[index]
                    .ranges
                    .iter()
                    .any(|range| overlaps(&page, range))
            }) {
                index += 1;
            } else {
                ready.push(state.waiters.swap_remove(index).callback);
            }
        }
        drop(state);
        for callback in ready {
            callback();
        }
    }

    /// Runs a notification after all loans overlapping `ranges` close.
    ///
    /// The callback runs without the tracker mutex. Recheck admission when
    /// handling the notification: a new loan can start before it is processed.
    pub fn notify_when_unlocked(
        &self,
        ranges: Vec<Range<u64>>,
        callback: Box<dyn FnOnce() + Send + Sync>,
    ) {
        let mut state = self.state.lock();
        if state.loans.iter().flat_map(|loan| loan.iter()).any(|&gpn| {
            let page = page_range(gpn).expect("reserved page range is valid");
            ranges.iter().any(|range| overlaps(&page, range))
        }) {
            state.waiters.push(UnlockWaiter { ranges, callback });
        } else {
            drop(state);
            callback();
        }
    }

    /// Atomically checks a complete batch and excludes new overlapping loans.
    ///
    /// Keep the returned guard through the operation that changes mappings or
    /// permissions, including backend calls that can revoke access themselves.
    /// No mutex is held by the guard.
    pub fn begin_change(
        self: &Arc<Self>,
        ranges: &[Range<u64>],
    ) -> Result<GuestMemoryChangeGuard, GuestMemoryUserLockError> {
        if ranges.is_empty() || ranges.iter().any(|range| range.start >= range.end) {
            return Err(GuestMemoryUserLockError::InvalidRange);
        }
        let mut stored = Vec::new();
        stored
            .try_reserve_exact(ranges.len())
            .map_err(GuestMemoryUserLockError::Allocation)?;
        stored.extend_from_slice(ranges);
        let mut state = self.state.lock();
        for &gpn in state.loans.iter().flat_map(|loan| loan.iter()) {
            let page = page_range(gpn)?;
            if ranges.iter().any(|range| overlaps(&page, range)) {
                return Err(GuestMemoryUserLockError::UserLocked(page.start));
            }
        }
        let id = state
            .next_change
            .checked_add(1)
            .ok_or(GuestMemoryUserLockError::InvalidRange)?;
        state
            .changes
            .try_reserve(1)
            .map_err(GuestMemoryUserLockError::Allocation)?;
        state.next_change = id;
        state.changes.push(Change {
            id,
            ranges: stored.into_boxed_slice(),
        });
        Ok(GuestMemoryChangeGuard {
            locks: self.clone(),
            id,
        })
    }
}

/// Prevents new usermode loans while a mapping/permission change is in flight.
#[derive(Debug)]
pub struct GuestMemoryChangeGuard {
    locks: Arc<GuestMemoryUserLocks>,
    id: u64,
}

impl Drop for GuestMemoryChangeGuard {
    fn drop(&mut self) {
        let mut state = self.locks.state.lock();
        let index = state
            .changes
            .iter()
            .position(|change| change.id == self.id)
            .expect("guest-memory change was not reserved");
        state.changes.swap_remove(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn overlapping_loans_keep_pages_locked_until_every_owner_unlocks() {
        let locks = Arc::new(GuestMemoryUserLocks::default());
        locks.lock_gpns(&[0, 0, 1]).unwrap();
        locks.lock_gpns(&[1]).unwrap();
        assert!(locks.begin_change(&[0..8192]).is_err());
        locks.unlock_gpns(&[0, 0, 1]);
        assert!(locks.begin_change(&[4096..8192]).is_err());
        locks.begin_change(&[0..4096]).unwrap();
        locks.unlock_gpns(&[1]);
        locks.begin_change(&[0..8192]).unwrap();
    }

    #[test]
    fn change_excludes_new_loans_without_blocking_unrelated_pages() {
        let locks = Arc::new(GuestMemoryUserLocks::default());
        let change = locks.begin_change(&[0..4096, 8192..12288]).unwrap();
        assert!(locks.lock_gpns(&[0]).is_err());
        assert!(locks.lock_gpns(&[2]).is_err());
        locks.begin_change(&[1024..2048]).unwrap();
        locks.lock_gpns(&[1]).unwrap();
        drop(change);
        locks.lock_gpns(&[0, 2]).unwrap();
        locks.unlock_gpns(&[0, 2]);
        locks.unlock_gpns(&[1]);
    }

    #[test]
    fn failed_batch_does_not_reserve_its_unlocked_prefix() {
        let locks = Arc::new(GuestMemoryUserLocks::default());
        locks.lock_gpns(&[1]).unwrap();
        assert!(locks.begin_change(&[0..4096, 4096..8192]).is_err());
        locks.lock_gpns(&[0]).unwrap();
        locks.unlock_gpns(&[0]);
        locks.unlock_gpns(&[1]);
        locks.begin_change(&[0..8192]).unwrap();
    }

    #[test]
    fn removal_notification_waits_for_last_overlap_and_runs_without_mutex() {
        let locks = Arc::new(GuestMemoryUserLocks::default());
        locks.lock_gpns(&[0, 1]).unwrap();
        locks.lock_gpns(&[1]).unwrap();
        let notified = Arc::new(std::sync::atomic::AtomicBool::new(false));
        locks.notify_when_unlocked(
            vec![0..8192],
            Box::new({
                let locks = locks.clone();
                let notified = notified.clone();
                move || {
                    locks.begin_change(&[0..8192]).unwrap();
                    notified.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }),
        );
        locks.unlock_gpns(&[0, 1]);
        assert!(!notified.load(std::sync::atomic::Ordering::Relaxed));
        locks.unlock_gpns(&[1]);
        assert!(notified.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn rejects_overflow_and_invalid_ranges_without_state_changes() {
        let locks = Arc::new(GuestMemoryUserLocks::default());
        assert!(locks.lock_gpns(&[u64::MAX]).is_err());
        assert!(locks.begin_change(&[]).is_err());
        assert!(locks.begin_change(&[4096..4096]).is_err());
        assert!(
            locks
                .begin_change(&[Range {
                    start: 4096,
                    end: 0
                }])
                .is_err()
        );
        assert!(locks.state.lock().loans.is_empty());
        assert!(locks.state.lock().changes.is_empty());
    }
}
