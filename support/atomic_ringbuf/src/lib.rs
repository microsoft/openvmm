// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A fixed-size, multi-reader, seqlock-based, atomic ring buffer.
//!
//! This data structure is useful in situations where multiple writers are
//! producing partial updates to a list of items and multiple readers are
//! consuming these updates, so long as the readers have a fallback method
//! for when they miss an update and the ring buffer wraps around. Writing
//! is synchronized with a lock, but read attempts are never blocked.

#![forbid(unsafe_code)]

cfg_if::cfg_if! {
    if #[cfg(all(target_arch = "x86_64", test))] {
        use loom::sync::Mutex;
        use loom::sync::MutexGuard;
        use loom::sync::atomic::AtomicU64;
        use loom::sync::atomic::AtomicUsize;
        use loom::sync::atomic::Ordering;
        use loom::sync::atomic::fence;
    } else {
        use parking_lot::Mutex;
        use parking_lot::MutexGuard;
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use std::sync::atomic::fence;
    }
}

use std::num::Wrapping;

// TODO: Support data sizes other than u64.

#[derive(Debug)]
#[cfg_attr(not(test), derive(inspect::Inspect))]
/// A fixed-size, multi-reader, seqlock-based, atomic ring buffer.
pub struct AtomicRingBuffer<const N: usize, T: Copy + From<u64> + Into<u64>> {
    /// The contents of the buffer.
    #[cfg_attr(not(test), inspect(hex, with = "|x| inspect::iter_by_index(x.iter())"))]
    buffer: Box<[AtomicU64; N]>,
    /// The number of items that have been added over the lifetime of the struct.
    list_count: AtomicUsize,
    /// The number of items that have started being added over the lifetime of
    /// the struct.
    in_progress_count: AtomicUsize,
    /// A guard to ensure that only one thread is writing at a time.
    write_lock: Mutex<()>,
    /// A marker to indicate the type of data stored in the buffer.
    _datatype: std::marker::PhantomData<T>,
}

/// A guard that allows writing to the ring buffer.
pub struct AtomicRingBufferWriteGuard<'a, const N: usize, T: Copy + From<u64> + Into<u64>> {
    buf: &'a AtomicRingBuffer<N, T>,
    _write_lock: MutexGuard<'a, ()>,
}

impl<const N: usize, T: Copy + From<u64> + Into<u64>> AtomicRingBuffer<N, T> {
    /// Creates a new `AtomicRingBuffer`.
    pub fn new() -> Self {
        Self {
            buffer: Box::new(std::array::from_fn(|_| AtomicU64::new(0))),
            list_count: AtomicUsize::new(0),
            in_progress_count: AtomicUsize::new(0),
            write_lock: Mutex::new(()),
            _datatype: std::marker::PhantomData,
        }
    }

    /// Returns the number of items that have been added to the buffer over its
    /// lifetime.
    ///
    /// This number only increases and is never reset, but it may wrap eventually.
    pub fn count(&self) -> Wrapping<usize> {
        Wrapping(self.list_count.load(Ordering::Acquire))
    }

    /// Obtain a write lock for the buffer.
    pub fn write(&self) -> AtomicRingBufferWriteGuard<'_, N, T> {
        let write_lock = self.write_lock.lock();
        #[cfg(all(target_arch = "x86_64", test))]
        let write_lock = write_lock.unwrap();
        AtomicRingBufferWriteGuard {
            buf: self,
            _write_lock: write_lock,
        }
    }

    /// Attempt to copy a portion of the buffer into the provided slice.
    ///
    /// This will copy `output.len()` items starting from `start_count` in the
    /// buffer. `start_count` must be a previously observed value of `count()`.
    /// If the buffer has wrapped around and items have been missed this will
    /// return `false`, and readers should use their fallback method. If no
    /// items have been missed it will return `true`, and readers may proceed
    /// to consume the partial update.
    pub fn try_copy(&self, start_count: usize, output: &mut [T]) -> bool {
        if output.len() > N {
            return false;
        }
        let mut index = start_count;
        for slot in output.iter_mut() {
            *slot = self.buffer[index % N].load(Ordering::Relaxed).into();
            index = index.wrapping_add(1);
        }
        fence(Ordering::Acquire);

        // Check to see whether any additional entries have been added
        // that would have caused a wraparound. If so, the local list is
        // incomplete and the copy has failed.
        if (self
            .in_progress_count
            .load(Ordering::Acquire)
            .wrapping_sub(start_count))
            > N
        {
            return false;
        }
        true
    }
}

impl<const N: usize, T: Copy + From<u64> + Into<u64>> AtomicRingBufferWriteGuard<'_, N, T> {
    /// Add an item to the buffer.
    ///
    /// Note: using [`Self::extend`] is more efficient than using this method in a loop.
    pub fn push(&self, item: T) {
        self.extend(std::iter::once(item));
    }

    /// Add a range of items to the buffer.
    pub fn extend(&self, items: impl ExactSizeIterator<Item = T>) {
        debug_assert_eq!(
            self.buf.in_progress_count.load(Ordering::Relaxed),
            self.buf.list_count.load(Ordering::Relaxed)
        );
        // Adding a new item to the buffer must be done in three steps:
        // 1. Indicate that an entry is about to be added so that any readers
        //    code executing simultaneously will know that they might lose an
        //    entry that they are expecting to see.
        // 2. Add the entry.
        // 3. Increment the valid entry count so that any readers executing
        //    simultaneously will know it is valid.
        let len = items.len();
        let start_count = self.buf.in_progress_count.load(Ordering::Relaxed);
        let end_count = start_count.wrapping_add(len);
        self.buf
            .in_progress_count
            .store(end_count, Ordering::Release);
        for (i, v) in items.enumerate() {
            self.buf.buffer[(start_count.wrapping_add(i)) % N].store(v.into(), Ordering::Release);
        }
        self.buf.list_count.store(end_count, Ordering::Release);
    }
}

#[cfg(all(target_arch = "x86_64", test))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;
    use loom::thread;

    fn loom_test<F>(f: F)
    where
        F: Fn() + Sync + Send + 'static,
    {
        let mut model = loom::model::Builder::new();
        // We need to set the preemption bound in order for any
        // possibly-endless loops to complete in a reasonable time.
        // Per loom's documentation: "In practice, setting the thread pre-emption
        // bound to 2 or 3 is enough to catch most bugs".
        model.preemption_bound = Some(2);
        model.check(f);
    }

    #[test]
    fn loom_basic() {
        loom_test(|| {
            const N: usize = 3;
            let buf = Arc::new(AtomicRingBuffer::<N, u64>::new());

            let buf_r = buf.clone();
            let reader = thread::spawn(move || {
                let mut out = [0u64; N];
                let mut start = 0;

                while start < N {
                    let next_start = buf_r.count().0;
                    if next_start == start {
                        thread::yield_now();
                        continue;
                    }
                    let ok = buf_r.try_copy(start, &mut out[start..next_start]);
                    assert!(ok);
                    start = next_start;
                }
                assert_eq!(out, [1, 2, 3]);
            });

            let buf_w = buf.clone();
            let writer = thread::spawn(move || {
                buf_w.write().push(1);
                buf_w.write().push(2);
                buf_w.write().push(3);
            });

            writer.join().unwrap();
            reader.join().unwrap();
        });
    }

    #[test]
    fn loom_multi_writer_wraparound() {
        loom_test(|| {
            let buf = Arc::new(AtomicRingBuffer::<3, u64>::new());

            let buf_w = buf.clone();
            let writer1 = thread::spawn(move || {
                buf_w.write().push(1);
                buf_w.write().push(2);
            });

            let buf_w = buf.clone();
            let writer2 = thread::spawn(move || {
                buf_w.write().push(3);
                buf_w.write().push(4);
            });

            let buf_r = buf.clone();
            let reader = thread::spawn(move || {
                let mut out = [0u64; 4];
                let mut start = 0;

                while start < 4 {
                    let next_start = buf_r.count().0;
                    if next_start == start {
                        thread::yield_now();
                        continue;
                    }
                    let ok = buf_r.try_copy(start, &mut out[start..next_start]);
                    if !ok {
                        // A wrap around has occurred, at least one of the last
                        // two items being written must be in the buffer. It is
                        // possible the last one is still being added.
                        assert!(buf_r.try_copy(1, &mut out[0..3]));
                        out[3] = 0;
                        assert!(out.contains(&2) || out.contains(&4));
                        return;
                    }
                    start = next_start;
                }
                // The reader should have seen every item, but we don't know the order
                assert!(out.contains(&1));
                assert!(out.contains(&2));
                assert!(out.contains(&3));
                assert!(out.contains(&4));
            });

            writer1.join().unwrap();
            writer2.join().unwrap();
            reader.join().unwrap();

            // Check that the ring buffer has wrapped around.
            assert!(!buf.try_copy(0, &mut []));
        });
    }
}
