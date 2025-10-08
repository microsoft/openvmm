// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements a vmbus ring buffer backed by a MemoryBlock,
//! allocated memory that has been locked to physical pages and whose pfns are
//! known.

use safeatomic::AtomicSliceOps;
use std::sync::atomic::AtomicU32;
use user_driver::memory::MemoryBlock;
use vmbus_ring::CONTROL_WORD_COUNT;
use vmbus_ring::PAGE_SIZE;
use vmbus_ring::RingMem;

pub struct MemoryBlockRingBuffer(MemoryBlock);

impl MemoryBlockRingBuffer {
    pub fn new(mem: MemoryBlock) -> Self {
        assert!(mem.len() >= 2 * PAGE_SIZE);
        Self(mem)
    }
}

impl From<MemoryBlock> for MemoryBlockRingBuffer {
    fn from(mem: MemoryBlock) -> Self {
        Self::new(mem)
    }
}

impl RingMem for MemoryBlockRingBuffer {
    fn len(&self) -> usize {
        self.0.len() - PAGE_SIZE
    }

    fn read_at(&self, addr: usize, data: &mut [u8]) {
        let addr = addr % self.len();
        let initial_size = usize::min(data.len(), self.len() - addr);
        self.0.read_at(addr + PAGE_SIZE, &mut data[..initial_size]);
        if initial_size < data.len() {
            self.0.read_at(PAGE_SIZE, &mut data[initial_size..]);
        }
    }

    fn write_at(&self, addr: usize, data: &[u8]) {
        let addr = addr % self.len();
        let initial_size = usize::min(data.len(), self.len() - addr);
        self.0.write_at(addr + PAGE_SIZE, &data[..initial_size]);
        if initial_size < data.len() {
            self.0.write_at(PAGE_SIZE, &data[initial_size..]);
        }
    }

    fn control(&self) -> &[AtomicU32; CONTROL_WORD_COUNT] {
        self.0.as_slice().as_atomic_slice().unwrap()[..CONTROL_WORD_COUNT]
            .try_into()
            .unwrap()
    }
}
