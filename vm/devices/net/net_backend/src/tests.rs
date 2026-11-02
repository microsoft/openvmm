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
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
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
    mergeable: bool,
}

struct BufsInner {
    rx_metadata: Vec<Mutex<Option<RxMetadata>>>,
    rx_capacity: Vec<AtomicU32>,
    guest_memory: GuestMemory,
}

impl Bufs {
    pub fn new(guest_memory: GuestMemory) -> Self {
        let mut rx_metadata = Vec::new();
        rx_metadata.resize_with(128, Default::default);
        let mut rx_capacity = Vec::new();
        rx_capacity.resize_with(128, || AtomicU32::new(2048));
        Self {
            mergeable: false,
            inner: Arc::new(BufsInner {
                rx_metadata,
                rx_capacity,
                guest_memory,
            }),
        }
    }
}

impl BufferAccess for Bufs {
    fn guest_memory(&self) -> &GuestMemory {
        &self.inner.guest_memory
    }

    fn push_guest_addresses(&self, id: RxId, buf: &mut Vec<RxBufferSegment>) {
        let gpa = id.0 as u64 * 2048;
        buf.push(RxBufferSegment { gpa, len: 2048 });
    }

    fn capacity(&self, id: RxId) -> u32 {
        self.inner.rx_capacity[id.0 as usize].load(Ordering::Relaxed)
    }

    fn supports_multiple_buffers(&self) -> bool {
        self.mergeable
    }

    fn write_packet_across_multiple_buffers(
        &mut self,
        ids: &[RxId],
        metadata: &RxMetadata,
        segments: &[&[u8]],
    ) -> bool {
        assert!(self.mergeable);
        let data: Vec<u8> = segments.iter().flat_map(|s| s.iter().copied()).collect();
        assert_eq!(data.len(), metadata.len);
        let mut data = data.as_slice();
        for &id in ids {
            let (part, rest) = data.split_at(data.len().min(self.capacity(id) as usize));
            self.write_data(id, part);
            data = rest;
        }
        assert!(data.is_empty());
        self.write_header(ids[0], metadata);
        true
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

impl Bufs {
    /// Enables multi-buffer receive support for backend tests.
    pub fn with_mergeable_buffers(mut self) -> Self {
        self.mergeable = true;
        self
    }

    /// Returns the [`RxMetadata`] written for the given receive buffer, if any.
    pub fn rx_metadata(&self, id: RxId) -> Option<RxMetadata> {
        *self.inner.rx_metadata[id.0 as usize].lock()
    }

    /// Overrides the capacity reported for a receive buffer.
    pub fn set_capacity(&self, id: RxId, capacity: u32) {
        self.inner.rx_capacity[id.0 as usize].store(capacity, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod receive_tests {
    use super::*;
    use std::collections::VecDeque;
    use test_with_tracing::test;

    #[test]
    fn mergeable_capacity_and_completion_boundaries() {
        let mem = GuestMemory::allocate(0x10000);
        let mut pool = Bufs::new(mem.clone()).with_mergeable_buffers();
        for (id, capacity) in [(7, 0), (3, 32), (1, 32)] {
            pool.set_capacity(RxId(id), capacity);
        }
        let mut available = VecDeque::from([RxId(7), RxId(3), RxId(1)]);
        let too_large = [0x11; 65];
        assert!(
            crate::write_rx_packet(
                &mut pool,
                &mut available,
                &RxMetadata {
                    len: 65,
                    ..Default::default()
                },
                &[&too_large]
            )
            .is_none()
        );
        assert_eq!(
            available.iter().map(|id| id.0).collect::<Vec<_>>(),
            [7, 3, 1]
        );
        assert!(pool.rx_metadata(RxId(7)).is_none());

        available.push_back(RxId(2));
        let data: Vec<u8> = (0..64).collect();
        let ids = crate::write_rx_packet(
            &mut pool,
            &mut available,
            &RxMetadata {
                len: 64,
                ..Default::default()
            },
            &[&[], &data[..19], &data[19..], &[]],
        )
        .unwrap();
        assert_eq!(ids.iter().map(|id| id.0).collect::<Vec<_>>(), [7, 3, 1]);
        assert_eq!(available.front().unwrap().0, 2);
        let mut ready = VecDeque::from([ids]);
        assert!(crate::drain_rx_packets(&mut ready, &mut [RxId(0); 2]).is_empty());
        let mut output = [RxId(0); 4];
        let completions = crate::drain_rx_packets(&mut ready, &mut output);
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].id.0, 7);
        assert_eq!(completions[0].buffer_count.get(), 3);
        assert_eq!(
            output[..3].iter().map(|id| id.0).collect::<Vec<_>>(),
            [7, 3, 1]
        );
        let mut readback = [0; 64];
        mem.read_at(3 * 2048, &mut readback[..32]).unwrap();
        mem.read_at(2048, &mut readback[32..]).unwrap();
        assert_eq!(readback.as_slice(), data);
    }

    #[test]
    fn nonmergeable_receive_never_skips_or_combines_buffers() {
        let mut pool = Bufs::new(GuestMemory::allocate(0x10000));
        pool.set_capacity(RxId(7), 32);
        let mut available = VecDeque::from([RxId(7), RxId(1)]);
        assert!(
            crate::write_rx_packet(
                &mut pool,
                &mut available,
                &RxMetadata {
                    len: 64,
                    ..Default::default()
                },
                &[&[0; 64]]
            )
            .is_none()
        );
        assert_eq!(available.len(), 2);
        let ids = crate::write_rx_packet(
            &mut pool,
            &mut available,
            &RxMetadata {
                len: 32,
                ..Default::default()
            },
            &[&[0; 32]],
        )
        .unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].0, 7);
        assert_eq!(available.front().unwrap().0, 1);
    }
}
