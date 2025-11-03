// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types to access work, completion, and event queues.

use crate::save_restore::CqEqSavedState;
use crate::save_restore::DoorbellSavedState;
use crate::save_restore::WqSavedState;
use gdma_defs::CLIENT_OOB_8;
use gdma_defs::CLIENT_OOB_24;
use gdma_defs::CLIENT_OOB_32;
use gdma_defs::CqEqDoorbellValue;
use gdma_defs::Cqe;
use gdma_defs::DB_CQ;
use gdma_defs::DB_EQ;
use gdma_defs::DB_RQ;
use gdma_defs::DB_SQ;
use gdma_defs::Eqe;
use gdma_defs::GdmaQueueType;
use gdma_defs::OWNER_BITS;
use gdma_defs::OWNER_MASK;
use gdma_defs::Sge;
use gdma_defs::WQE_ALIGNMENT;
use gdma_defs::WqDoorbellValue;
use gdma_defs::WqeHeader;
use gdma_defs::WqeParams;
use inspect::Inspect;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering::Acquire;
use user_driver::memory::MemoryBlock;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// An interface to write a doorbell value to signal the device.
pub trait Doorbell: Send + Sync {
    /// Returns the maximum page number.
    fn page_count(&self) -> u32;
    /// Write a doorbell value at page `page`, offset `address`.
    fn write(&self, page: u32, address: u32, value: u64);
    /// Save the doorbell state.
    fn save(&self, doorbell_id: Option<u64>) -> DoorbellSavedState;
}

struct NullDoorbell;

impl Doorbell for NullDoorbell {
    fn page_count(&self) -> u32 {
        0
    }

    fn write(&self, _page: u32, _address: u32, _value: u64) {}

    fn save(&self, _doorbell_id: Option<u64>) -> DoorbellSavedState {
        DoorbellSavedState {
            doorbell_id: 0,
            page_count: 0,
        }
    }
}

/// A single GDMA doorbell page.
#[derive(Clone)]
pub struct DoorbellPage {
    doorbell: Arc<dyn Doorbell>,
    doorbell_id: u32,
}

impl DoorbellPage {
    pub(crate) fn null() -> Self {
        Self {
            doorbell: Arc::new(NullDoorbell),
            doorbell_id: 0,
        }
    }

    /// Returns a doorbell page at `doorbell_id` the doorbell region.
    pub fn new(doorbell: Arc<dyn Doorbell>, doorbell_id: u32) -> anyhow::Result<Self> {
        let page_count = doorbell.page_count();
        if doorbell_id >= page_count {
            anyhow::bail!(
                "doorbell id {} exceeds page count {}",
                doorbell_id,
                page_count
            );
        }
        Ok(Self {
            doorbell,
            doorbell_id,
        })
    }

    /// Writes a doorbell value.
    pub fn write(&self, address: u32, value: u64) {
        assert!(address < 4096);
        self.doorbell.write(self.doorbell_id, address, value);
    }
}

/// An event queue.
pub struct CqEq<T> {
    doorbell: DoorbellPage,
    doorbell_addr: u32,
    queue_type: GdmaQueueType,
    mem: MemoryBlock,
    id: u32,
    next: u32,
    size: u32,
    shift: u32,
    _phantom: PhantomData<fn() -> T>,
}

impl<T> Inspect for CqEq<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field("id", self.id)
            .hex("size", self.size)
            .hex("next", self.next);
    }
}

impl CqEq<Cqe> {
    /// Creates a new completion queue.
    pub fn new_cq(mem: MemoryBlock, doorbell: DoorbellPage, id: u32) -> Self {
        Self::new(GdmaQueueType::GDMA_CQ, DB_CQ, mem, doorbell, id)
    }

    /// Restores an existing completion queue.
    pub fn restore_cq(mem: MemoryBlock, state: CqEqSavedState, doorbell: DoorbellPage) -> Self {
        Self::restore(GdmaQueueType::GDMA_CQ, mem, doorbell, state)
    }
}

impl CqEq<Eqe> {
    /// Creates a new event queue.
    pub fn new_eq(mem: MemoryBlock, doorbell: DoorbellPage, id: u32) -> Self {
        Self::new(GdmaQueueType::GDMA_EQ, DB_EQ, mem, doorbell, id)
    }

    /// Restores an existing event queue.
    pub fn restore_eq(mem: MemoryBlock, state: CqEqSavedState, doorbell: DoorbellPage) -> Self {
        Self::restore(GdmaQueueType::GDMA_EQ, mem, doorbell, state)
    }
}

impl<T: IntoBytes + FromBytes + Immutable + KnownLayout> CqEq<T> {
    /// Creates a new queue.
    fn new(
        queue_type: GdmaQueueType,
        doorbell_addr: u32,
        mem: MemoryBlock,
        doorbell: DoorbellPage,
        id: u32,
    ) -> Self {
        let size = mem.len();
        assert!(size.is_power_of_two());
        Self {
            doorbell,
            doorbell_addr,
            queue_type,
            mem,
            id,
            next: size as u32,
            size: size as u32,
            shift: size.trailing_zeros(),
            _phantom: PhantomData,
        }
    }

    /// Save the state of the queue for restoration after servicing.
    pub fn save(&self) -> CqEqSavedState {
        CqEqSavedState {
            doorbell: DoorbellSavedState {
                doorbell_id: self.doorbell.doorbell_id as u64,
                page_count: self.doorbell.doorbell.page_count(),
            },
            doorbell_addr: self.doorbell_addr,
            id: self.id,
            next: self.next,
            size: self.size,
            shift: self.shift,
        }
    }

    /// Restore a queue from saved state.
    pub fn restore(
        queue_type: GdmaQueueType,
        mem: MemoryBlock,
        doorbell: DoorbellPage,
        state: CqEqSavedState,
    ) -> Self {
        Self {
            doorbell,
            doorbell_addr: state.doorbell_addr,
            queue_type,
            mem,
            id: state.id,
            next: state.next,
            size: state.size,
            shift: state.shift,
            _phantom: PhantomData,
        }
    }

    /// Updates the queue ID.
    pub(crate) fn set_id(&mut self, id: u32) {
        self.id = id;
    }

    /// Updates the doorbell page.
    pub(crate) fn set_doorbell(&mut self, page: DoorbellPage) {
        self.doorbell = page;
    }

    /// Gets the queue ID.
    pub fn id(&self) -> u32 {
        self.id
    }

    fn read_next<U: FromBytes + Immutable + KnownLayout>(&self, offset: u32) -> U {
        assert!((offset as usize & (size_of::<T>() - 1)) + size_of::<U>() <= size_of::<T>());
        self.mem
            .read_obj((self.next.wrapping_add(offset) & (self.size - 1)) as usize)
    }

    /// Pops an event queue entry.
    pub fn pop(&mut self) -> Option<T> {
        // Perform an acquire load to ensure that the read of the queue entry is
        // not reordered before the read of the owner count.
        let b = self.mem.as_slice()
            [(self.next.wrapping_add(size_of::<T>() as u32 - 1) & (self.size - 1)) as usize]
            .load(Acquire);
        let owner_count = b >> 5;
        let cur_owner_count = (self.next >> self.shift) as u8;
        if owner_count == (cur_owner_count.wrapping_sub(1)) & OWNER_MASK as u8 {
            None
        } else if owner_count == cur_owner_count & OWNER_MASK as u8 {
            let qe = self.read_next::<T>(0);
            self.next = self.next.wrapping_add(size_of_val(&qe) as u32);
            Some(qe)
        } else {
            tracing::error!(next = self.next, owner_count, queue_type = ?self.queue_type, id = self.id, "eq/cq wrapped");
            None
        }
    }

    fn flush(&mut self, arm: bool) {
        let tail = self.next & ((self.size << OWNER_BITS) - 1);
        let value = CqEqDoorbellValue::new()
            .with_arm(arm)
            .with_id(self.id)
            .with_tail(tail / size_of::<T>() as u32);
        tracing::trace!(queue_type = ?self.queue_type, id = self.id, ?value, "cq/eq doorbell write");
        self.doorbell.write(self.doorbell_addr, value.into());
    }

    /// Arms the event queue so that an interrupt will be delivered next time an
    /// event arrives.
    pub fn arm(&mut self) {
        self.flush(true);
    }

    /// Ack's the queue. Interrupt will not be delivered until it is armed.
    pub fn ack(&mut self) {
        self.flush(false);
    }

    /// Reports next value for diagnostics
    pub fn get_next(&mut self) -> u32 {
        self.next
    }
}

/// A completion queue.
pub type Cq = CqEq<Cqe>;

/// An event queue.
pub type Eq = CqEq<Eqe>;

/// A work queue (send or receive).
pub struct Wq {
    doorbell: DoorbellPage,
    queue_type: GdmaQueueType,
    doorbell_addr: u32,
    mem: MemoryBlock,
    id: u32,
    head: u32,
    tail: u32,
    mask: u32,
    uncommitted_count: u32,
}

impl Inspect for Wq {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field("id", self.id)
            .hex("size", self.mask + 1)
            .hex("head", self.head)
            .hex("tail", self.tail)
            .field("uncommited", self.uncommitted_count);
    }
}

/// An error indicating the queue is full.
#[derive(Debug)]
pub struct QueueFull;

impl Wq {
    /// Creates a new send work queue.
    pub fn new_sq(mem: MemoryBlock, doorbell: DoorbellPage, id: u32) -> Self {
        Self::new(GdmaQueueType::GDMA_SQ, DB_SQ, mem, doorbell, id)
    }

    /// Creates a new receive work queue.
    pub fn new_rq(mem: MemoryBlock, doorbell: DoorbellPage, id: u32) -> Self {
        Self::new(GdmaQueueType::GDMA_RQ, DB_RQ, mem, doorbell, id)
    }

    /// Creates a new work queue.
    fn new(
        queue_type: GdmaQueueType,
        doorbell_addr: u32,
        mem: MemoryBlock,
        doorbell: DoorbellPage,
        id: u32,
    ) -> Self {
        let size = mem.len() as u32;
        assert!(size.is_power_of_two());
        Self {
            doorbell,
            queue_type,
            doorbell_addr,
            mem,
            id,
            head: size,
            tail: 0,
            mask: size - 1,
            uncommitted_count: 0,
        }
    }

    /// Save the state of the Wq for restoration after servicing
    pub fn save(&self) -> WqSavedState {
        WqSavedState {
            doorbell: DoorbellSavedState {
                doorbell_id: self.doorbell.doorbell_id as u64,
                page_count: self.doorbell.doorbell.page_count(),
            },
            doorbell_addr: self.doorbell_addr,
            id: self.id,
            head: self.head,
            tail: self.tail,
            mask: self.mask,
        }
    }

    /// Restores an existing receive work queue.
    pub fn restore_rq(
        mem: MemoryBlock,
        state: WqSavedState,
        doorbell: DoorbellPage,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            doorbell,
            doorbell_addr: state.doorbell_addr,
            queue_type: GdmaQueueType::GDMA_RQ,
            mem,
            id: state.id,
            head: state.head,
            tail: state.tail,
            mask: state.mask,
            uncommitted_count: 0,
        })
    }

    /// Restores an existing send work queue.
    pub fn restore_sq(
        mem: MemoryBlock,
        state: WqSavedState,
        doorbell: DoorbellPage,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            doorbell,
            doorbell_addr: state.doorbell_addr,
            queue_type: GdmaQueueType::GDMA_SQ,
            mem,
            id: state.id,
            head: state.head,
            tail: state.tail,
            mask: state.mask,
            uncommitted_count: 0,
        })
    }

    /// Returns the queue ID.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Advances the head, indicating that `n` more bytes are available in the ring.
    pub fn advance_head(&mut self, n: u32) {
        assert!(n.is_multiple_of(WQE_ALIGNMENT as u32));
        self.head = self.head.wrapping_add(n);
    }

    fn get_offset_in_buffer_in_bytes(&self, offset: u32) -> usize {
        (offset as usize * WQE_ALIGNMENT) & self.mask as usize
    }

    /// Reads from the offset, the first `n` bytes.
    pub fn read(&mut self, offset: u32, n: usize) -> Vec<u8> {
        let mut buf = vec![0; n];
        let offset_in_buffer = self.get_offset_in_buffer_in_bytes(offset);
        self.mem.read_at(offset_in_buffer, &mut buf);
        buf
    }

    fn write_tail(&self, offset: u32, data: &[u8]) {
        debug_assert!(
            offset as usize % WQE_ALIGNMENT + data.len() <= WQE_ALIGNMENT,
            "can't write more than one queue segment at a time to avoid wrapping"
        );
        self.mem
            .write_at((self.tail.wrapping_add(offset) & self.mask) as usize, data);
    }

    /// Returns the number of bytes available in the ring.
    pub fn available(&self) -> u32 {
        self.head.wrapping_sub(self.tail)
    }

    /// Pushes a new work queue entry with an inline out-of-band buffer and
    /// external data via a scatter-gather list.
    pub fn push<I: IntoIterator<Item = Sge>>(
        &mut self,
        oob: impl IntoBytes + Immutable + KnownLayout,
        sgl: I,
    ) -> Result<u32, QueueFull> {
        let mut builder = self.wqe_builder(oob);
        for sge in sgl {
            builder.push_sge(sge);
        }
        builder.finish()
    }

    /// Begins building a work queue entry with an inline out-of-band buffer.
    pub fn wqe_builder(&mut self, oob: impl IntoBytes + Immutable + KnownLayout) -> WqeBuilder<'_> {
        WqeBuilder::new(self, oob)
    }

    /// Commits all written entries by updating the doorbell value observed by
    /// the device.
    pub fn commit(&mut self) {
        // N.B. the tail is not masked to the queue size.
        let mut value = WqDoorbellValue::new().with_id(self.id).with_tail(self.tail);
        if self.queue_type == GdmaQueueType::GDMA_RQ {
            // If this overflows, it's probably for a device type (like bnic)
            // that ignores it.
            value.set_num_rwqe(self.uncommitted_count as u8);
        }
        tracing::trace!(queue_type = ?self.queue_type, id = self.id, ?value, "wq doorbell write");
        self.doorbell.write(self.doorbell_addr, value.into());
        self.uncommitted_count = 0;
    }

    /// Reports tail value for diagnostics
    pub fn get_tail(&mut self) -> u32 {
        self.tail
    }
}

/// A builder for a work queue entry.
pub struct WqeBuilder<'a> {
    wq: &'a mut Wq,
    len: u32,
    max: u32,
    hdr: WqeHeader,
}

impl<'a> WqeBuilder<'a> {
    fn new(wq: &'a mut Wq, oob: impl IntoBytes + Immutable + KnownLayout) -> Self {
        let oob_size = match size_of_val(&oob) {
            0 | 8 => CLIENT_OOB_8,
            24 => CLIENT_OOB_24,
            32 => CLIENT_OOB_32,
            _ => panic!("invalid oob size"),
        };
        let max = wq.available();
        let len =
            (size_of::<WqeHeader>() + size_of_val(&oob)).next_multiple_of(size_of::<Sge>()) as u32;

        // Save the header to write later.
        let hdr = WqeHeader {
            reserved: [0; 3],
            last_vbytes: 0,
            params: WqeParams::new().with_inline_client_oob_size(oob_size),
        };

        // Write the out-of-band data.
        if len <= max {
            match size_of_val(&oob) {
                0 => {}
                8 | 24 => {
                    wq.write_tail(8, oob.as_bytes());
                }
                32 => {
                    wq.write_tail(8, &oob.as_bytes()[..24]);
                    wq.write_tail(32, &oob.as_bytes()[24..]);
                }
                _ => unreachable!(),
            }
        }

        Self { wq, len, max, hdr }
    }

    /// Sets that client out-of-band data is provided in the first scatter-gather
    /// entry, with the specified `last_vbytes` value.
    pub fn set_client_oob_in_sgl(&mut self, last_vbytes: u8) {
        self.hdr.last_vbytes = last_vbytes;
        self.hdr.params.set_client_oob_in_sgl(true);
    }

    /// Sets the `gd_client_unit_data` field in the header.
    pub fn set_gd_client_unit_data(&mut self, value: u16) {
        self.hdr.params.set_gd_client_unit_data(value);
    }

    /// Appends a scatter-gather entry to the work queue entry.
    pub fn push_sge(&mut self, sge: Sge) {
        let offset = self.len;
        self.len += size_of_val(&sge) as u32;
        if self.len <= self.max {
            self.wq.write_tail(offset, sge.as_bytes());
        }
        self.hdr
            .params
            .set_num_sgl_entries(self.hdr.params.num_sgl_entries() + 1);
    }

    /// Returns the number of scatter-gather entries added so far.
    pub fn sge_count(&self) -> u8 {
        self.hdr.params.num_sgl_entries()
    }

    /// Finishes building the work queue entry and writes it to the queue,
    /// updating the tail and returning the total length of the entry.
    ///
    /// Call [`Wq::commit`] to notify the device of the new entry.
    pub fn finish(self) -> Result<u32, QueueFull> {
        let aligned_len = self.len.next_multiple_of(WQE_ALIGNMENT as u32);
        if aligned_len > self.max {
            return Err(QueueFull);
        }
        self.wq.write_tail(0, self.hdr.as_bytes());
        self.wq.tail = self.wq.tail.wrapping_add(aligned_len);
        self.wq.uncommitted_count += 1;
        Ok(aligned_len)
    }
}
