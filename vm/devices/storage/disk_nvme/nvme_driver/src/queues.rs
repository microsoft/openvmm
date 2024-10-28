// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of submission and completion queues.

use super::spec;
use crate::registers::DeviceRegisters;
use inspect::Inspect;
use mesh::payload::Protobuf;
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
            base_mem: self.mem.base_va(),
            pfns: self.mem.pfns().to_vec(),
        }
    }

    /// Restores queue data after servicing.
    pub fn restore(&mut self, saved_state: &SubmissionQueueSavedState) -> anyhow::Result<()> {
        self.sqid = saved_state.sqid;
        self.head = saved_state.head;
        self.tail = saved_state.tail;
        self.committed_tail = saved_state.committed_tail;
        self.len = saved_state.len;
        Ok(())
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
            base_mem: self.mem.base_va(),
            pfns: self.mem.pfns().to_vec(),
        }
    }

    /// Restores queue data after servicing.
    pub fn restore(&mut self, saved_state: &CompletionQueueSavedState) -> anyhow::Result<()> {
        self.cqid = saved_state.cqid;
        self.head = saved_state.head;
        self.committed_head = saved_state.committed_head;
        self.len = saved_state.len;
        self.phase = saved_state.phase;
        Ok(())
    }
}

fn advance(n: u32, l: u32) -> u32 {
    if n + 1 < l {
        n + 1
    } else {
        0
    }
}

#[derive(Protobuf, Clone, Debug)]
#[mesh(package = "underhill")]
pub struct SubmissionQueueSavedState {
    #[mesh(1)]
    pub sqid: u16,
    #[mesh(2)]
    pub head: u32,
    #[mesh(3)]
    pub tail: u32,
    #[mesh(4)]
    pub committed_tail: u32,
    #[mesh(5)]
    pub len: u32,
    #[mesh(7)]
    pub base_mem: u64,
    #[mesh(8)]
    pub pfns: Vec<u64>,
}

#[derive(Protobuf, Clone, Debug)]
#[mesh(package = "underhill")]
pub struct CompletionQueueSavedState {
    #[mesh(1)]
    pub cqid: u16,
    #[mesh(2)]
    pub head: u32,
    #[mesh(3)]
    pub committed_head: u32,
    #[mesh(4)]
    pub len: u32,
    #[mesh(5)]
    /// NVMe completion tag.
    pub phase: bool,
    #[mesh(7)]
    pub base_mem: u64,
    #[mesh(8)]
    pub pfns: Vec<u64>,
}
