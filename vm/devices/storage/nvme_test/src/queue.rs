// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe submission and completion queue types.

use crate::DOORBELL_STRIDE_BITS;
use crate::spec;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::task::ready;
use thiserror::Error;
use vmcore::interrupt::Interrupt;

pub struct DoorbellMemory {
    mem: GuestMemory,
    offset: u64,
    event_idx_offset: Option<u64>,
    wakers: Vec<Option<Waker>>,
}

pub struct InvalidDoorbell;

impl DoorbellMemory {
    pub fn new(num_qids: u16) -> Self {
        Self {
            mem: GuestMemory::allocate((num_qids as usize) << DOORBELL_STRIDE_BITS),
            offset: 0,
            event_idx_offset: None,
            wakers: (0..num_qids).map(|_| None).collect(),
        }
    }

    /// Update the memory used to store the doorbell values. This is used to
    /// support shadow doorbells, where the values are directly in guest memory.
    pub fn replace_mem(
        &mut self,
        mem: GuestMemory,
        offset: u64,
        event_idx_offset: Option<u64>,
    ) -> Result<(), GuestMemoryError> {
        // Copy the current doorbell values into the new memory.
        let len = self.wakers.len() << DOORBELL_STRIDE_BITS;
        let mut current = vec![0; len];
        self.mem.read_at(self.offset, &mut current)?;
        mem.write_at(offset, &current)?;
        if let Some(event_idx_offset) = event_idx_offset {
            // Catch eventidx up to the current doorbell value.
            mem.write_at(event_idx_offset, &current)?;
        }
        self.mem = mem;
        self.offset = offset;
        self.event_idx_offset = event_idx_offset;
        Ok(())
    }

    pub fn try_write(&self, db_id: u16, value: u32) -> Result<(), InvalidDoorbell> {
        if (db_id as usize) >= self.wakers.len() {
            return Err(InvalidDoorbell);
        }
        self.write(db_id, value);
        Ok(())
    }

    fn write(&self, db_id: u16, value: u32) {
        assert!((db_id as usize) < self.wakers.len());
        let addr = self
            .offset
            .wrapping_add((db_id as u64) << DOORBELL_STRIDE_BITS);
        if let Err(err) = self.mem.write_plain(addr, &value) {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "failed to write doorbell memory"
            );
        }
        if let Some(waker) = &self.wakers[db_id as usize] {
            waker.wake_by_ref();
        }
    }

    fn read(&self, db_id: u16) -> Option<u32> {
        assert!((db_id as usize) < self.wakers.len());
        self.mem
            .read_plain(
                self.offset
                    .wrapping_add((db_id as u64) << DOORBELL_STRIDE_BITS),
            )
            .inspect_err(|err| {
                tracelimit::error_ratelimited!(
                    error = err as &dyn std::error::Error,
                    "failed to read doorbell memory"
                );
            })
            .ok()
    }

    fn has_event_idx(&self) -> bool {
        self.event_idx_offset.is_some()
    }

    fn write_event_idx(&self, db_id: u16, val: u32) {
        assert!((db_id as usize) < self.wakers.len());
        if let Err(err) = self.mem.write_plain(
            self.event_idx_offset
                .unwrap()
                .wrapping_add((db_id as u64) << DOORBELL_STRIDE_BITS),
            &val,
        ) {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "failed to read event_idx memory"
            )
        }
    }

    fn read_event_idx(&self, db_id: u16) -> Option<u32> {
        assert!((db_id as usize) < self.wakers.len());
        self.mem
            .read_plain(
                self.event_idx_offset?
                    .wrapping_add((db_id as u64) << DOORBELL_STRIDE_BITS),
            )
            .inspect_err(|err| {
                tracelimit::error_ratelimited!(
                    error = err as &dyn std::error::Error,
                    "failed to read doorbell memory"
                );
            })
            .ok()
    }
}

#[derive(Inspect)]
#[inspect(extra = "Self::inspect_shadow")]
struct DoorbellState {
    #[inspect(hex)]
    current: u32,
    #[inspect(hex)]
    event_idx: u32,
    db_id: u16,
    db_offset: u64,
    #[inspect(hex)]
    len: u32,
    #[inspect(skip)]
    doorbells: Arc<RwLock<DoorbellMemory>>,
    #[inspect(skip)]
    registered_waker: Option<Waker>,
}

impl DoorbellState {
    fn inspect_shadow(&self, resp: &mut inspect::Response<'_>) {
        resp.field_with("doorbell", || {
            self.doorbells.read().read(self.db_id).map(inspect::AsHex)
        })
        .field_with("shadow_event_idx", || {
            self.doorbells
                .read()
                .read_event_idx(self.db_id)
                .map(inspect::AsHex)
        });
    }

    fn new(doorbells: Arc<RwLock<DoorbellMemory>>, db_id: u16, len: u32) -> Self {
        Self {
            current: 0,
            event_idx: 0,
            len,
            doorbells,
            registered_waker: None,
            db_id,
            db_offset: (db_id as u64) << DOORBELL_STRIDE_BITS,
        }
    }

    fn probe_inner(&mut self, update_event_idx: bool) -> Option<u32> {
        // Try to read forward.
        let doorbell = self.doorbells.read();
        let val = doorbell.read(self.db_id)?;
        if val != self.current {
            return Some(val);
        }

        if self.event_idx == val || !update_event_idx || !doorbell.has_event_idx() {
            return None;
        }

        // Update the event index so that the guest will write the real doorbell
        // on the next update.
        doorbell.write_event_idx(self.db_id, val);
        self.event_idx = val;

        // Double check after a memory barrier.
        std::sync::atomic::fence(Ordering::SeqCst);
        let val = doorbell.read(self.db_id)?;
        if val != self.current { Some(val) } else { None }
    }

    fn probe(&mut self, update_event_idx: bool) -> Result<bool, QueueError> {
        // If shadow doorbells are in use, use that instead of what was written to the doorbell
        // register, as it may be more current.
        if let Some(val) = self.probe_inner(update_event_idx) {
            if val >= self.len {
                return Err(QueueError::InvalidDoorbell { val, len: self.len });
            }
            self.current = val;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), QueueError>> {
        // Ensure we get woken up whenever the doorbell is written to.
        if self
            .registered_waker
            .as_ref()
            .is_none_or(|w| !cx.waker().will_wake(w))
        {
            let _old_waker =
                self.doorbells.write().wakers[self.db_id as usize].replace(cx.waker().clone());
            self.registered_waker = Some(cx.waker().clone());
        }
        if !self.probe(true)? {
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }
}

#[derive(Inspect)]
pub struct SubmissionQueue {
    tail: DoorbellState,
    mem: GuestMemory,
    #[inspect(hex)]
    head: u32,
    #[inspect(hex)]
    gpa: u64,
}

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("invalid doorbell value {val:#x}, len {len:#x}")]
    InvalidDoorbell { val: u32, len: u32 },
    #[error("queue access error")]
    Memory(#[source] GuestMemoryError),
}

impl SubmissionQueue {
    pub fn new(
        doorbells: Arc<RwLock<DoorbellMemory>>,
        db_id: u16,
        gpa: u64,
        len: u16,
        mem: GuestMemory,
    ) -> Self {
        doorbells.read().write(db_id, 0);
        Self {
            tail: DoorbellState::new(doorbells, db_id, len.into()),
            head: 0,
            gpa,
            mem,
        }
    }

    /// This function returns a future for the next entry in the submission queue.  It also
    /// has a side effect of updating the tail.
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Result<spec::Command, QueueError>> {
        let tail = self.tail.current;
        if tail == self.head {
            ready!(self.tail.poll(cx))?;
        }
        let command: spec::Command = self
            .mem
            .read_plain(
                self.gpa
                    .wrapping_add(self.head as u64 * size_of::<spec::Command>() as u64),
            )
            .map_err(QueueError::Memory)?;

        self.head = advance(self.head, self.tail.len);
        Poll::Ready(Ok(command))
    }

    pub fn sqhd(&self) -> u16 {
        self.head as u16
    }
}

#[derive(Inspect)]
pub struct CompletionQueue {
    #[inspect(hex)]
    tail: u32,
    head: DoorbellState,
    phase: bool,
    #[inspect(hex)]
    gpa: u64,
    #[inspect(with = "Option::is_some")]
    interrupt: Option<Interrupt>,
    mem: GuestMemory,
}

impl CompletionQueue {
    pub fn new(
        doorbells: Arc<RwLock<DoorbellMemory>>,
        db_id: u16,
        mem: GuestMemory,
        interrupt: Option<Interrupt>,
        gpa: u64,
        len: u16,
    ) -> Self {
        doorbells.read().write(db_id, 0);
        Self {
            tail: 0,
            head: DoorbellState::new(doorbells, db_id, len.into()),
            phase: true,
            gpa,
            interrupt,
            mem,
        }
    }

    /// Wait for free completions.
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), QueueError>> {
        let next_tail = advance(self.tail, self.head.len);
        if self.head.current == next_tail {
            ready!(self.head.poll(cx))?;
        }
        Poll::Ready(Ok(()))
    }

    pub fn write(&mut self, mut data: spec::Completion) -> Result<bool, QueueError> {
        let next = advance(self.tail, self.head.len);
        // Check the doorbell register instead of requiring the caller to
        // go around the slow path and call `poll_ready`.
        if self.head.current == next && !self.head.probe(false)? {
            return Ok(false);
        }
        data.status.set_phase(self.phase);

        // Atomically write the low part of the completion entry first, then the
        // high part, using release fences to ensure ordering.
        //
        // This is necessary to ensure the guest can observe the full completion
        // once it observes the phase bit change (which is in the high part).
        let [low, high]: [u64; 2] = zerocopy::transmute!(data);
        let gpa = self
            .gpa
            .wrapping_add(self.tail as u64 * size_of::<spec::Completion>() as u64);
        self.mem
            .write_plain(gpa, &low)
            .map_err(QueueError::Memory)?;
        std::sync::atomic::fence(Ordering::Release);
        self.mem
            .write_plain(gpa + 8, &high)
            .map_err(QueueError::Memory)?;
        std::sync::atomic::fence(Ordering::Release);

        if let Some(interrupt) = &self.interrupt {
            interrupt.deliver();
        }
        self.tail = next;
        if self.tail == 0 {
            self.phase = !self.phase;
        }
        Ok(true)
    }
}

fn advance(n: u32, l: u32) -> u32 {
    if n + 1 < l { n + 1 } else { 0 }
}
