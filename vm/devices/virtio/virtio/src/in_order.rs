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
//! instantiate it and pay nothing — the queue's hot path is untouched. Consumed
//! descriptors are recorded in consumption order, completions fill their slot
//! (possibly out of order), and only the contiguous filled prefix is published
//! to the used ring, in order.
//!
//! A driver that makes a split-ring descriptor head available again before
//! its previous use has been published violates the virtio spec. The reused
//! head is left in the available ring until the earlier use is published and
//! then processed normally. Packed-ring overlap is rejected, since publishing
//! the earlier completion overwrites the reused descriptor metadata. Use
//! [`InOrderCompletion::poll_kick`] to wait without spinning on a deferred head.

use crate::VirtioQueue;
use crate::VirtioQueueCallbackWork;
use crate::common::PeekResult;
use crate::queue::QueueCompletion;
use inspect::Inspect;
use std::collections::VecDeque;
use std::io::Error;
use std::task::Context;
use std::task::Poll;
use std::task::ready;

/// Enforces in-order used-ring publication for a single [`VirtioQueue`].
///
/// Consume descriptors via [`try_next`](Self::try_next) and complete them via
/// [`complete`](Self::complete) instead of calling the queue's methods
/// directly. Wait for work via [`poll_kick`](Self::poll_kick). Completions may be
/// issued in any order; the used ring is always published in consumption
/// (available) order.
#[derive(Inspect)]
pub struct InOrderCompletion {
    /// Descriptor indices of outstanding descriptors, in consumption order.
    /// The front is the next descriptor eligible to be published.
    #[inspect(with = "VecDeque::len")]
    order: VecDeque<u16>,
    /// An ID remains in use until its completion is published, including while
    /// the completion is buffered behind earlier work.
    #[inspect(skip)]
    in_flight: Vec<bool>,
    /// Completed-but-not-yet-published work, indexed by descriptor index.
    /// Bounded by the queue size, so a descriptor index (always
    /// `< queue_size`) is a unique key for the outstanding set: a repeated
    /// index while still outstanding is deferred by `try_next`.
    #[inspect(skip)]
    completed: Vec<Option<Completed>>,
    /// Outstanding count when fetching required a completion to retire.
    /// Between fetches, `order` can only shrink, even if publication fails.
    /// Cleared by the next fetch, never maintained by completion paths.
    blocked_at_outstanding: Option<usize>,
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
            in_flight: vec![false; queue_size as usize],
            completed: (0..queue_size).map(|_| None).collect(),
            blocked_at_outstanding: None,
        }
    }

    /// Consumes the next available descriptor from `queue`, recording it for
    /// in-order completion. Returns `Ok(None)` if no work is available.
    ///
    /// If the next available descriptor head is still outstanding, it is left
    /// unconsumed and `Ok(None)` is returned until the earlier use is
    /// published. A split ring with no remaining head capacity is likewise
    /// deferred. Packed chains exceeding the remaining capacity fail the queue.
    ///
    /// Use this in place of [`VirtioQueue::try_next`].
    pub fn try_next(
        &mut self,
        queue: &mut VirtioQueue,
    ) -> Result<Option<VirtioQueueCallbackWork>, Error> {
        self.blocked_at_outstanding = None;
        let work = match queue.try_peek_with_backpressure()? {
            PeekResult::Empty => return Ok(None),
            PeekResult::CapacityBlocked => {
                self.blocked_at_outstanding = Some(self.order.len());
                return Ok(None);
            }
            PeekResult::Work(work) => work,
        };
        let idx = work.descriptor_index();
        // The queue validates the descriptor index against the ring size
        // when it reads the descriptor, so an out-of-range index fails
        // above rather than reaching here. Assert the invariant so a
        // violation fails fast at entry instead of corrupting later state.
        assert!(
            (idx as usize) < self.completed.len(),
            "descriptor index {idx} exceeds queue size {}",
            self.completed.len()
        );
        if self.in_flight[idx as usize] {
            drop(work);
            self.blocked_at_outstanding = Some(self.order.len());
            tracelimit::event_ratelimited_static!(
                level: DEBUG,
                descriptor_index = idx,
                "guest reused an outstanding descriptor, deferring it"
            );
            return Ok(None);
        }
        self.in_flight[idx as usize] = true;
        self.order.push_back(idx);
        Ok(Some(work.consume()))
    }

    /// Waits for guest work that can be consumed by [`try_next`](Self::try_next).
    ///
    /// Unlike [`VirtioQueue::poll_kick`], this returns `Pending` if fetching
    /// encountered reuse or insufficient capacity and no outstanding entry has
    /// retired since. Retirement permits a retry even if publication failed.
    /// Callers must also poll their completion source with the same waker and
    /// retry fetching after processing completions: this method does not
    /// register a waker for retirement.
    pub fn poll_kick(&self, queue: &mut VirtioQueue, cx: &mut Context<'_>) -> Poll<()> {
        ready!(queue.poll_kick(cx));
        if self.blocked_at_outstanding == Some(self.order.len()) {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
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
        if self.order.pop_front_if(|front| *front == idx).is_some() {
            queue.complete_prepared(completion, bytes_written);
            self.in_flight[idx as usize] = false;
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
            self.in_flight[front as usize] = false;
        }
    }

    /// Returns the number of descriptors consumed but not yet published.
    pub fn outstanding(&self) -> usize {
        self.order.len()
    }
}
