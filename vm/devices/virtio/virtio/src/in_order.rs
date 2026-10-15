// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! In-order completion discipline layered on top of a [`VirtioQueue`].
//!
//! Some devices (notably virtio-net) require that the used ring be published
//! strictly in the order descriptors were consumed from the available ring,
//! even though work may complete out of order (e.g. a network backend that
//! finishes packets in a different order, or descriptors that are dropped
//! early). This makes the outstanding set exactly the contiguous
//! `[used_index, avail_index)` range, which enables a simple cursor-based
//! save/restore.
//!
//! This is deliberately a *side* helper whose methods borrow a
//! [`VirtioQueue`], rather than being baked into the queue itself. Devices that
//! do not need in-order completion (virtio-blk, virtio-vsock, ...) never
//! instantiate it and pay nothing — the queue's hot path is untouched. It uses
//! only the queue's public API, mirroring QEMU's
//! `virtqueue_ordered_fill`/`virtqueue_ordered_flush` (`used_elems`): consumed
//! descriptors are recorded in consumption order, completions fill their slot
//! (possibly out of order), and only the contiguous filled prefix is published
//! to the used ring, in order.

use crate::VirtioQueue;
use crate::VirtioQueueCallbackWork;
use crate::queue::QueueCompletion;
use inspect::Inspect;
use std::collections::VecDeque;
use std::io::Error;

/// Enforces in-order used-ring publication for a single [`VirtioQueue`].
///
/// Consume descriptors via [`try_next`](Self::try_next) and complete them via
/// [`complete`](Self::complete) instead of calling the queue's methods
/// directly. Completions may be issued in any order; the used ring is always
/// published in consumption (available) order.
#[derive(Inspect)]
pub struct InOrderCompletion {
    /// Descriptor indices of outstanding descriptors, in consumption order.
    /// The front is the next descriptor eligible to be published.
    #[inspect(with = "VecDeque::len")]
    order: VecDeque<u16>,
    /// Completed-but-not-yet-published work, indexed by descriptor index.
    /// Bounded by the queue size, so a descriptor index (always
    /// `< queue_size`) is a unique key for the outstanding set: a repeated
    /// index while still outstanding is a guest protocol violation that the
    /// device detects and treats as fatal.
    #[inspect(skip)]
    completed: Vec<Option<Completed>>,
}

struct Completed {
    completion: QueueCompletion,
    bytes_written: u32,
}

impl InOrderCompletion {
    /// Creates an in-order completion tracker for a queue of the given size.
    pub fn new(queue_size: u16) -> Self {
        Self {
            order: VecDeque::with_capacity(queue_size as usize),
            completed: (0..queue_size).map(|_| None).collect(),
        }
    }

    /// Consumes the next available descriptor from `queue`, recording it for
    /// in-order completion. Returns `Ok(None)` if no work is available.
    ///
    /// Use this in place of [`VirtioQueue::try_next`].
    pub fn try_next(
        &mut self,
        queue: &mut VirtioQueue,
    ) -> Result<Option<VirtioQueueCallbackWork>, Error> {
        let work = queue.try_next()?;
        if let Some(work) = &work {
            let idx = work.descriptor_index() as usize;
            // The queue validates the descriptor index against the ring size
            // when it reads the descriptor, so an out-of-range index fails
            // above rather than reaching here. Assert the invariant so a
            // violation fails fast at entry instead of corrupting later state.
            assert!(
                idx < self.completed.len(),
                "descriptor index {idx} exceeds queue size {}",
                self.completed.len()
            );
            self.order.push_back(work.descriptor_index());
        }
        Ok(work)
    }

    /// Records a completion for the descriptor identified by `completion` and
    /// publishes the contiguous run of completed descriptors at the front of
    /// the consumption order to the used ring, in order.
    ///
    /// `completion` is the lightweight token obtained from
    /// [`VirtioQueueCallbackWork::into_completion`], so callers that buffer
    /// outstanding descriptors can retain only the token rather than the full
    /// work (and its payload).
    ///
    /// Use this in place of [`VirtioQueue::complete_prepared`].
    ///
    /// Each `completion` must be for a descriptor still outstanding from
    /// [`try_next`](Self::try_next), completed once. Some violations panic (an
    /// out-of-range index, or a collision with a still-buffered slot); a
    /// completion for a no-longer-outstanding descriptor is not caught and
    /// would strand an entry in `completed`, eventually stalling publication.
    pub fn complete(
        &mut self,
        queue: &mut VirtioQueue,
        completion: QueueCompletion,
        bytes_written: u32,
    ) {
        let idx = completion.descriptor_index();

        // Fast path: the completing descriptor is the one at the front of the
        // consumption order — the overwhelmingly common case, since an ordered
        // backend completes in the order buffers were posted. Publish it
        // directly, with no `completed` slot write/read. (When `idx` is at the
        // front, its slot is guaranteed empty: any completed front descriptor is
        // published and popped immediately, so the front is always outstanding
        // on entry.)
        if self.order.front() == Some(&idx) {
            self.order.pop_front();
            queue.complete_prepared(completion, bytes_written);
            if !self.order.is_empty() {
                self.drain_completed_prefix(queue);
            }
            return;
        }

        // Slow path: an out-of-order completion. Buffer it until the
        // descriptors ahead of it have been published. Because `idx` is not at
        // the front, nothing new can be published yet.
        let slot = self
            .completed
            .get_mut(idx as usize)
            .expect("completed descriptor index must be within the queue size");
        assert!(
            slot.is_none(),
            "descriptor {idx} completed more than once while outstanding"
        );
        *slot = Some(Completed {
            completion,
            bytes_written,
        });
    }

    /// Publishes the longest run of already-completed descriptors at the front
    /// of the consumption order, in order.
    fn drain_completed_prefix(&mut self, queue: &mut VirtioQueue) {
        while let Some(&front) = self.order.front() {
            let Some(Completed {
                completion,
                bytes_written,
            }) = self.completed[front as usize].take()
            else {
                break;
            };
            self.order.pop_front();
            queue.complete_prepared(completion, bytes_written);
        }
    }

    /// Returns the number of descriptors consumed but not yet published.
    pub fn outstanding(&self) -> usize {
        self.order.len()
    }
}
