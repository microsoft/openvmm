// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of submission and completion queues.

use super::spec;
use crate::driver::save_restore::CompletionQueueSavedState;
use crate::driver::save_restore::SubmissionQueueSavedState;
use crate::registers::DeviceRegisters;
use inspect::Inspect;
use user_driver::memory::MemoryBlock;
use user_driver::DeviceBacking;

#[derive(Inspect)]
pub(crate) struct SubmissionQueue {
    sqid: u16,
    head: u32,
    tail: u32,
    committed_tail: u32,
    len: u32,
    #[inspect(skip)]
    mem: MemoryBlock,
}

#[derive(Debug)]
pub(crate) struct QueueFull;

impl SubmissionQueue {
    pub fn new(sqid: u16, len: u16, mem: MemoryBlock) -> Self {
        Self {
            sqid,
            head: 0,
            tail: 0,
            committed_tail: 0,
            len: len.into(),
            mem,
        }
    }

    pub fn id(&self) -> u16 {
        self.sqid
    }

    pub fn update_head(&mut self, head: u16) {
        let head = head as u32;
        assert!(head < self.len);
        self.head = head;
    }

    pub fn is_full(&self) -> bool {
        advance(self.tail, self.len) == self.head
    }

    pub fn write(&mut self, command: spec::Command) -> Result<(), QueueFull> {
        let next_tail = advance(self.tail, self.len);
        if next_tail == self.head {
            return Err(QueueFull);
        }
        self.mem
            .write_obj(self.tail as usize * size_of_val(&command), &command);
        self.tail = next_tail;
        Ok(())
    }

    pub fn commit<T: DeviceBacking>(&mut self, region: &DeviceRegisters<T>) {
        if self.tail != self.committed_tail {
            safe_intrinsics::store_fence();
            region.doorbell(self.sqid, false, self.tail);
            self.committed_tail = self.tail;
        }
    }

    /// Saves queue data for servicing.
    pub fn save(&self) -> SubmissionQueueSavedState {
        SubmissionQueueSavedState {
            sqid: self.sqid,
            head: self.head,
            tail: self.tail,
            committed_tail: self.committed_tail,
            len: self.len,
        }
    }

    /// Restores queue data after servicing.
    pub fn restore(
        mem: MemoryBlock,
        saved_state: &SubmissionQueueSavedState,
    ) -> anyhow::Result<Self> {
        let SubmissionQueueSavedState {
            sqid,
            head,
            tail,
            committed_tail,
            len,
        } = saved_state;
        Ok(Self {
            sqid: *sqid,
            head: *head,
            tail: *tail,
            committed_tail: *committed_tail,
            len: *len,
            mem,
        })
    }
}

#[derive(Inspect)]
pub(crate) struct CompletionQueue {
    cqid: u16,
    head: u32,
    committed_head: u32,
    /// Queue size in entries.
    len: u32,
    phase: bool,
    #[inspect(skip)]
    mem: MemoryBlock,
}

impl CompletionQueue {
    pub fn new(cqid: u16, len: u16, mem: MemoryBlock) -> CompletionQueue {
        Self {
            cqid,
            head: 0,
            committed_head: 0,
            len: len.into(),
            phase: true,
            mem,
        }
    }

    pub fn _id(&self) -> u16 {
        self.cqid
    }

    pub fn read(&mut self) -> Option<spec::Completion> {
        let completion = self
            .mem
            .read_obj::<spec::Completion>(self.head as usize * size_of::<spec::Completion>());
        if completion.status.phase() != self.phase {
            return None;
        }
        self.head += 1;
        if self.head == self.len {
            self.head = 0;
            self.phase = !self.phase;
        }
        Some(completion)
    }

    pub fn commit<T: DeviceBacking>(&mut self, registers: &DeviceRegisters<T>) {
        if self.head != self.committed_head {
            safe_intrinsics::store_fence();
            registers.doorbell(self.cqid, true, self.head);
            self.committed_head = self.head;
        }
    }

    /// Saves queue data for servicing.
    pub fn save(&self) -> CompletionQueueSavedState {
        CompletionQueueSavedState {
            cqid: self.cqid,
            head: self.head,
            committed_head: self.committed_head,
            len: self.len,
            phase: self.phase,
        }
    }

    /// Restores queue data after servicing.
    pub fn restore(
        mem: MemoryBlock,
        saved_state: &CompletionQueueSavedState,
    ) -> anyhow::Result<Self> {
        let CompletionQueueSavedState {
            cqid,
            head,
            committed_head,
            len,
            phase,
        } = saved_state;

        Ok(Self {
            cqid: *cqid,
            head: *head,
            committed_head: *committed_head,
            len: *len,
            phase: *phase,
            mem,
        })
    }
}

fn advance(n: u32, l: u32) -> u32 {
    if n + 1 < l {
        n + 1
    } else {
        0
    }
}
