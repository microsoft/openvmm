// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use alloc::vec::Vec;

/// Read-only vector of entries `T` that can be accessed concurrently by multiple threads.
/// Each entry is associated with an atomic boolean flag that is used to mark if the entry
/// is referenced by a thread. If an entry is referenced, it is considered owned by that thread
/// and cannot be referenced again.
/// Note that referenced / taken entries are not cleared from the vector until the entire vector is dropped.
/// This is to enable the vector to be "popped" by multiple threads concurrently.
pub(crate) struct AtomicRefQueue<T> {
    data: Vec<(T, AtomicBool)>,
    start_idx: AtomicUsize,
}

// AtomicRefQueue is safe to send between threads, as long as the entries are also Send.
// Same goes for sync.
unsafe impl<T> Send for AtomicRefQueue<T> where T: Send {}
unsafe impl<T> Sync for AtomicRefQueue<T> where T: Sync {}

impl<T> AtomicRefQueue<T> {
    /// Create a new AtomicRefQueue from a list of entries.
    pub(crate) fn new(list: Vec<T>) -> Self {
        // Convert the list of entries into a list of (entry, atomic flag) pairs
        let list = list
            .into_iter()
            .map(|entry| (entry, AtomicBool::new(false)))
            .collect();
        // Return the AtomicRefQueue
        AtomicRefQueue {
            data: list,
            start_idx: AtomicUsize::new(0),
        }
    }
    /// Searches for the next entry in the vector that hasn't already been taken / referenced
    /// and that passes the conditional function check.
    /// If found, marks the entry and returns a reference to the instruction to the callee.
    /// If no unmarked entry is found, returns None.
    /// Note: This is logically a conditional `pop` operation on a `Mutex<Vec<T>>`, but with a unique implementation
    /// due to the no_std requirement.
    pub(crate) fn pop_ref_conditional<F>(&self, conditional_func: F) -> Option<&T>
    where
        F: Fn(&T) -> bool,
    {
        // Track whether we should update the start idx, this gets set to false if we skip an entry
        // (i.e. if we don't pop out a sequential entry)
        let mut update_start_idx = true;
        let start_idx = self.start_idx.load(Ordering::SeqCst);
        for (idx, entry) in self.data[start_idx..].iter().enumerate() {
            // Check if the entry is already marked without marking it first
            // If it is not, then ensure it also passes the conditional function check
            if !entry.1.load(Ordering::SeqCst) {
                if conditional_func(&entry.0) {
                    // Passed initial checks, now try to mark the entry
                    if let Ok(false) =
                        entry
                            .1
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    {
                        // Marked it, return a reference to the entry and optionally update the start idx
                        if update_start_idx && idx > start_idx {
                            // Just use the current idx as the start idx, even though the current is currently being
                            // returned and marked. Prevents the need for checking edge cases e.g. if the current idx is the
                            // last.
                            //
                            // Also, if multiple threads are concurrently popping entries, they may both be writing to the
                            // start_idx. This is fine, as the start_idx will be valid regardless which thread writes to it.
                            self.start_idx.store(idx, Ordering::SeqCst);
                        }
                        return Some(&entry.0);
                    }
                } else {
                    // Failed the conditional function, we may not be processing entries sequentially. Ensure
                    // we do not update the start idx.
                    update_start_idx = false;
                }

                // Failed to mark the entry, continue iterating
            }
            // Failed to mark the entry or the initial checks, continue iterating
        }
        None
    }
}

// Add tests for the AtomicRefQueue, ensuring that the vector is correctly populated and that
// the pop_ref method returns the correct entries, and stops when the vector is all marked.
#[cfg(test)]
#[allow(non_snake_case)]
mod tests {

    use super::*;

    #[test]
    fn test_AtomicRefQueue_sequential_take() {
        let vec = AtomicRefQueue::new(vec![1, 2, 3]);
        fn always_true(_: &i32) -> bool {
            true
        }
        // After pushing, all entries should be available in order.
        assert_eq!(vec.pop_ref_conditional(always_true), Some(&1));
        assert_eq!(vec.pop_ref_conditional(always_true), Some(&2));
        assert_eq!(vec.pop_ref_conditional(always_true), Some(&3));
        // All entries are now marked, so further calls should return None.
        assert_eq!(vec.pop_ref_conditional(always_true), None);
    }

    #[test]
    fn test_AtomicRefQueue_non_sequential_take() {
        let vec = AtomicRefQueue::new(vec![1, 2, 3, 4]);
        fn always_true(_: &i32) -> bool {
            true
        }
        fn true_on_even(val: &i32) -> bool {
            *val % 2 == 0
        }
        fn always_false(_: &i32) -> bool {
            false
        }
        // After pushing, all entries should be available in order.
        assert_eq!(vec.pop_ref_conditional(true_on_even), Some(&2));
        assert_eq!(vec.pop_ref_conditional(always_true), Some(&1));
        assert_eq!(vec.pop_ref_conditional(always_true), Some(&3));
        assert_eq!(vec.pop_ref_conditional(always_false), None);
        // All entries are now marked, so further calls should return None.
        assert_eq!(vec.pop_ref_conditional(true_on_even), Some(&4));
        assert_eq!(vec.pop_ref_conditional(always_true), None);
    }

    #[test]
    fn test_AtomicRefQueue_empty() {
        let vec: AtomicRefQueue<i32> = AtomicRefQueue::new(Vec::new());
        fn always_true(_: &i32) -> bool {
            true
        }
        assert_eq!(vec.pop_ref_conditional(always_true), None);
    }
}
