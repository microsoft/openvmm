// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe submission and completion queue types.

use crate::spec;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use thiserror::Error;
use vmcore::interrupt::Interrupt;

pub const ILLEGAL_DOORBELL_VALUE: u32 = 0xffffffff;

#[derive(Default, Inspect)]
#[inspect(transparent)]
pub struct DoorbellRegister {
    #[inspect(with = "|x| inspect::AsHex(x.load(Ordering::Relaxed))")]
    value: AtomicU32,
    #[inspect(skip)]
    event: event_listener::Event,
}

impl DoorbellRegister {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write(&self, value: u32) {
        self.value.store(value, Ordering::SeqCst);
        self.event.notify(usize::MAX);
    }

    pub fn read(&self) -> u32 {
        self.value.load(Ordering::SeqCst)
    }

    pub async fn wait_read(&self, value: u32) -> u32 {
        let v = self.read();
        if value != v {
            return v;
        }
        loop {
            let listener = self.event.listen();
            let v = self.read();
            if value != v {
                break v;
            }
            listener.await;
        }
    }
}

#[derive(Copy, Clone, Default, Inspect, Debug)]
pub struct ShadowDoorbell {
    #[inspect(hex)]
    pub shadow_db_gpa: u64,
    #[inspect(hex)]
    pub event_idx_gpa: u64,
}

impl ShadowDoorbell {
    // See NVMe Spec version 2.0a, Section 5.8 -- Doorbell Buffer Config Command for
    // an explanation of this math.
    pub fn new(
        shadow_db_evt_idx_base: ShadowDoorbell,
        qid: u16,
        is_sq: bool,
        doorbell_stride_bits: usize,
    ) -> ShadowDoorbell {
        let offset = match is_sq {
            true => 0u64,
            false => 1u64,
        };
        let shadow_db_gpa = shadow_db_evt_idx_base.shadow_db_gpa
            + (qid as u64 * 2 + offset) * (4 << (doorbell_stride_bits - 2));
        let event_idx_gpa = shadow_db_evt_idx_base.event_idx_gpa
            + (qid as u64 * 2 + offset) * (4 << (doorbell_stride_bits - 2));
        ShadowDoorbell {
            shadow_db_gpa,
            event_idx_gpa,
        }
    }
}

#[derive(Inspect)]
pub struct SubmissionQueue {
    #[inspect(hex)]
    cached_tail: u32,
    tail: Arc<DoorbellRegister>,
    #[inspect(hex)]
    head: u32,
    #[inspect(hex)]
    gpa: u64,
    #[inspect(hex)]
    len: u32,
    #[inspect(with = "Option::is_some")]
    shadow_db_evt_idx: Option<ShadowDoorbell>,
    #[inspect(hex)]
    evt_idx: u32,
}

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("invalid doorbell tail {0:#x}")]
    InvalidTail(u32),
    #[error("invalid doorbell head {0:#x}")]
    InvalidHead(u32),
    #[error("queue access error")]
    Memory(#[source] GuestMemoryError),
}

impl SubmissionQueue {
    pub fn new(
        tail: Arc<DoorbellRegister>,
        gpa: u64,
        len: u16,
        shadow_db_evt_idx: Option<ShadowDoorbell>,
    ) -> Self {
        tail.write(0);
        Self {
            cached_tail: 0,
            tail,
            head: 0,
            gpa,
            len: len.into(),
            shadow_db_evt_idx,
            evt_idx: 0,
        }
    }

    /// This function returns a future for the next entry in the submission queue.  It also
    /// has a side effect of updating the tail.
    ///
    /// Note that this function returns a future that must be cancellable, which means that the
    /// parts after an await may never run.  The tail update side effect is benign, so
    /// that can happen before the await.
    pub async fn next(&mut self, mem: &GuestMemory) -> Result<spec::Command, QueueError> {
        // If shadow doorbells are in use, use that instead of what was written to the doorbell
        // register, as it may be more current.
        if let Some(shadow_db_evt_idx) = self.shadow_db_evt_idx {
            let shadow_tail = mem
                .read_plain(shadow_db_evt_idx.shadow_db_gpa)
                .map_err(QueueError::Memory)?;

            // ILLEGAL_DOORBELL_VALUE is the initial state.  The guest will overwrite
            // it when it first uses the shadow.
            if shadow_tail != ILLEGAL_DOORBELL_VALUE {
                self.cached_tail = shadow_tail;
                self.tail.write(self.cached_tail);
            }
        }
        while self.cached_tail == self.head {
            self.cached_tail = self.tail.wait_read(self.cached_tail).await;
        }
        if self.cached_tail >= self.len {
            return Err(QueueError::InvalidTail(self.cached_tail));
        }
        let command: spec::Command = mem
            .read_plain(self.gpa.wrapping_add(self.head as u64 * 64))
            .map_err(QueueError::Memory)?;

        self.head = advance(self.head, self.len);
        Ok(command)
    }

    pub fn sqhd(&self) -> u16 {
        self.head as u16
    }

    /// This function lets the driver know what doorbell value we consumed, allowing
    /// it to elide the next ring, maybe.
    pub fn advance_evt_idx(&mut self, mem: &GuestMemory) -> Result<(), QueueError> {
        self.evt_idx = advance(self.evt_idx, self.len);
        if let Some(shadow_db_evt_idx) = self.shadow_db_evt_idx {
            mem.write_plain(shadow_db_evt_idx.event_idx_gpa, &self.evt_idx)
                .map_err(QueueError::Memory)?;
        }
        Ok(())
    }

    /// This function updates the shadow doorbell values of a queue that is
    /// potentially already in use.
    pub fn update_shadow_db(&mut self, mem: &GuestMemory, sdb: ShadowDoorbell) {
        self.shadow_db_evt_idx = Some(sdb);
        self.evt_idx = self.cached_tail;
        // Write the illegal value out to the buffer, so that we can tell
        // if Linux has ever written a valid value.
        let _ = mem.write_plain(sdb.shadow_db_gpa, &ILLEGAL_DOORBELL_VALUE);
    }
}

#[derive(Inspect)]
pub struct CompletionQueue {
    #[inspect(hex)]
    tail: u32,
    #[inspect(hex)]
    cached_head: u32,
    head: Arc<DoorbellRegister>,
    phase: bool,
    #[inspect(hex)]
    gpa: u64,
    #[inspect(hex)]
    len: u32,
    #[inspect(with = "Option::is_some")]
    interrupt: Option<Interrupt>,
    shadow_db_evt_idx: Option<ShadowDoorbell>,
}

impl CompletionQueue {
    pub fn new(
        head: Arc<DoorbellRegister>,
        interrupt: Option<Interrupt>,
        gpa: u64,
        len: u16,
        shadow_db_evt_idx: Option<ShadowDoorbell>,
    ) -> Self {
        head.write(0);
        Self {
            tail: 0,
            cached_head: 0,
            head,
            phase: true,
            gpa,
            len: len.into(),
            interrupt,
            shadow_db_evt_idx,
        }
    }

    /// Wait for free completions.
    pub async fn wait_ready(&mut self, mem: &GuestMemory) -> Result<(), QueueError> {
        let next_tail = advance(self.tail, self.len);
        // If shadow doorbells are in use, use that instead of what was written to the doorbell
        // register, as it may be more current.
        if let Some(shadow_db_evt_idx) = self.shadow_db_evt_idx {
            let shadow_head = mem
                .read_plain(shadow_db_evt_idx.shadow_db_gpa)
                .map_err(QueueError::Memory)?;

            // ILLEGAL_DOORBELL_VALUE is the initial state.  The guest will overwrite
            // it when it first uses the shadow.
            if shadow_head != ILLEGAL_DOORBELL_VALUE {
                self.cached_head = shadow_head;
                self.head.write(self.cached_head);
            }
        }
        while self.cached_head == next_tail {
            self.cached_head = self.head.wait_read(self.cached_head).await;
        }
        if self.cached_head >= self.len {
            return Err(QueueError::InvalidHead(self.cached_head));
        }
        Ok(())
    }

    pub fn write(
        &mut self,
        mem: &GuestMemory,
        mut data: spec::Completion,
    ) -> Result<bool, QueueError> {
        if self.cached_head == advance(self.tail, self.len) {
            return Ok(false);
        }
        data.status.set_phase(self.phase);
        mem.write_plain(self.gpa.wrapping_add(self.tail as u64 * 16), &data)
            .map_err(QueueError::Memory)?;
        if let Some(interrupt) = &self.interrupt {
            interrupt.deliver();
        }
        self.tail = advance(self.tail, self.len);
        if self.tail == 0 {
            self.phase = !self.phase;
        }
        Ok(true)
    }

    /// This method updates the EVT_IDX field to match the shadow doorbell
    /// value, thus signalling to the guest driver that the next completion
    /// removed should involve a doorbell ring.  In this emulator, such
    /// a thing (the ring) is only necessary when the number of un-spoken-for
    /// completion queue entries is getting small.  (Completion queue entries
    /// are spoken for when a command is removed from the SQ).
    pub fn catch_up_evt_idx(
        &mut self,
        force: bool,
        io_outstanding: u32,
        mem: &GuestMemory,
    ) -> Result<(), QueueError> {
        if let Some(shadow_db_evt_idx) = self.shadow_db_evt_idx {
            if force | (io_outstanding >= self.len - 3) {
                mem.write_plain(shadow_db_evt_idx.event_idx_gpa, &self.cached_head)
                    .map_err(QueueError::Memory)?;
            }
        }
        Ok(())
    }

    /// This function updates the shadow doorbell values of a queue that is
    /// potentially already in use.
    pub fn update_shadow_db(&mut self, mem: &GuestMemory, sdb: ShadowDoorbell) {
        self.shadow_db_evt_idx = Some(sdb);
        // Write the illegal value out to the buffer, so that we can tell
        // if Linux has ever written a valid value.
        let _ = mem.write_plain(sdb.shadow_db_gpa, &ILLEGAL_DOORBELL_VALUE);
    }
}

fn advance(n: u32, l: u32) -> u32 {
    if n + 1 < l {
        n + 1
    } else {
        0
    }
}
