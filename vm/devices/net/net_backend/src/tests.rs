// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types to help test backends.

use crate::BufferAccess;
use crate::RxBufferSegment;
use crate::RxId;
use crate::RxMetadata;
use guestmem::GuestMemory;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use std::sync::Arc;
use vm_topology::memory::MemoryLayout;

pub fn test_layout() -> MemoryLayout {
    MemoryLayout::new(
        64 * 4096,
        &[
            MemoryRange::new(64 * 4096..65 * 4096),
            MemoryRange::new(65 * 4096..66 * 4096),
        ],
        &[],
        &[],
        None,
    )
    .unwrap()
}

#[derive(Clone)]
pub struct Bufs {
    inner: Arc<BufsInner>,
    buffer_segment: RxBufferSegment,
}

struct BufsInner {
    rx_metadata: Vec<Mutex<Option<RxMetadata>>>,
    guest_memory: GuestMemory,
}

impl Bufs {
    pub fn new(guest_memory: GuestMemory) -> Self {
        let mut rx_metadata = Vec::new();
        rx_metadata.resize_with(128, Default::default);
        Self {
            inner: Arc::new(BufsInner {
                rx_metadata,
                guest_memory,
            }),
            buffer_segment: RxBufferSegment { gpa: 0, len: 0 },
        }
    }
}

impl BufferAccess for Bufs {
    fn guest_memory(&self) -> &GuestMemory {
        &self.inner.guest_memory
    }

    fn guest_addresses(&mut self, id: RxId) -> &[RxBufferSegment] {
        let gpa = id.0 as u64 * 2048;
        self.buffer_segment = RxBufferSegment { gpa, len: 2048 };
        std::slice::from_ref(&self.buffer_segment)
    }

    fn capacity(&self, _id: RxId) -> u32 {
        2048
    }

    fn write_data(&mut self, id: RxId, buf: &[u8]) {
        self.inner
            .guest_memory
            .write_at(id.0 as u64 * 2048, buf)
            .unwrap();
    }

    fn write_header(&mut self, id: RxId, metadata: &RxMetadata) {
        *self.inner.rx_metadata[id.0 as usize].lock() = Some(*metadata);
    }
}
