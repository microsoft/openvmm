// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::dma::DmaRegion;
use gdma_defs::CqEqDoorbellValue;
use gdma_defs::Cqe;
use gdma_defs::CqeParams;
use gdma_defs::Eqe;
use gdma_defs::EqeParams;
use gdma_defs::GDMA_EQE_COMPLETION;
use gdma_defs::OWNER_BITS;
use gdma_defs::OWNER_MASK;
use gdma_defs::PAGE_SIZE64;
use gdma_defs::WQE_ALIGNMENT;
use gdma_defs::WqDoorbellValue;
use gdma_defs::Wqe;
use gdma_defs::WqeHeader;
use guestmem::GuestMemory;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use parking_lot::MappedMutexGuard;
use parking_lot::Mutex;
use parking_lot::MutexGuard;
use pci_core::capabilities::msix::MsixEmulator;
use std::marker::PhantomData;
use std::sync::atomic::Ordering::Release;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use thiserror::Error;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::VmTaskDriver;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// Offset the queue IDs seen by the guest.
const ID_OFFSET: usize = 24;

struct CqEq<T> {
    region: DmaRegion,
    shift: u32,
    cap: u32,
    tail: u32,
    armed: bool,
    _phantom: PhantomData<fn(T)>,
}

impl<T> Inspect for CqEq<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .hex("size", self.cap)
            .hex("tail", self.tail)
            .field("armed", self.armed);
    }
}

#[derive(Debug, Error)]
pub enum QueueAllocError {
    #[error("invalid queue alignment")]
    InvalidAlignment,
    #[error("invalid queue length")]
    InvalidLen,
    #[error("out of queues")]
    NoMoreQueues,
}

impl<T: IntoBytes + Immutable + KnownLayout> CqEq<T> {
    fn new(region: DmaRegion) -> Result<Self, QueueAllocError> {
        if !region.is_aligned_to(size_of::<T>()) {
            return Err(QueueAllocError::InvalidAlignment);
        }
        let len = region.len();
        if len < PAGE_SIZE64 as usize || len > u32::MAX as usize || !region.len().is_power_of_two()
        {
            return Err(QueueAllocError::InvalidLen);
        }

        let count = len as u32 / size_of::<T>() as u32;

        Ok(Self {
            region,
            shift: count.trailing_zeros(),
            cap: count,
            tail: count, // start with owner_count = 1
            armed: true, // start in armed state
            _phantom: PhantomData,
        })
    }

    fn owner_count(&self) -> u8 {
        ((self.tail >> self.shift) & OWNER_MASK) as u8
    }

    fn post(&mut self, gm: &GuestMemory, entry: &T) -> bool {
        let offset = (self.tail & (self.cap - 1)) as usize * size_of::<T>();
        let mut range = self.region.range();
        range.skip(offset);
        let mut writer = range.writer(gm);
        let (entry, last) = entry.as_bytes().split_at(entry.as_bytes().len() - 1);
        if let Err(err) = writer.write(entry) {
            tracing::warn!(
                err = &err as &dyn std::error::Error,
                "failed to write entry"
            );
        }
        // Write the final byte last after a release fence to ensure that the
        // guest sees the entire entry before the owner count is updated.
        std::sync::atomic::fence(Release);
        if let Err(err) = writer.write(last) {
            tracing::warn!(err = &err as &dyn std::error::Error, "failed to write last");
        }
        // Ensure the write is flushed before sending the interrupt.
        std::sync::atomic::fence(Release);
        let new_tail = self.tail.wrapping_add(1);
        self.tail = new_tail;
        std::mem::take(&mut self.armed)
    }

    fn doorbell(&mut self, tail: u32, arm: bool) -> bool {
        if arm {
            let n = self.tail.wrapping_sub(tail) & ((self.cap << OWNER_BITS) - 1);
            if n == 0 {
                // The guest's tail matches our tail, so arm the queue.
                self.armed = true;
                false
            } else if n <= self.cap {
                // The guest's tail does not match, so trigger the queue action
                // immediately.
                self.armed = false;
                true
            } else {
                // Overflow condition. It seems that real hardware skips
                // notifying in this scenario.
                tracing::warn!(
                    tail = self.tail,
                    doorbell = tail,
                    "invalid doorbell, overflow likely"
                );
                false
            }
        } else {
            self.armed = false;
            false
        }
    }
}

#[derive(Inspect)]
struct Cq {
    #[inspect(flatten)]
    q: CqEq<Cqe>,
    eq_id: u32,
}

struct Eq {
    q: CqEq<Eqe>,
    msix: u32,
}

impl Inspect for Eq {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond().field("msix", self.msix).merge(&self.q);
    }
}

struct Wq {
    region: DmaRegion,
    cap: u32,
    head: u32,
    tail: u32,
    waker: Option<Waker>,
}

impl Inspect for Wq {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .hex("size", self.cap)
            .hex("head", self.head)
            .hex("tail", self.tail);
    }
}

impl Wq {
    fn new(mut region: DmaRegion) -> Result<Self, QueueAllocError> {
        if !region.is_aligned_to(PAGE_SIZE64 as usize) {
            return Err(QueueAllocError::InvalidAlignment);
        }
        let len = region.len();
        if len > u32::MAX as usize || !region.len().is_power_of_two() {
            return Err(QueueAllocError::InvalidLen);
        }

        // Double up the region to make it easier to access WQEs that straddle
        // the end of the region.
        region.double();

        Ok(Self {
            region,
            cap: len as u32,
            head: 0,
            tail: 0,
            waker: None,
        })
    }

    fn poll_wqe(&mut self, gm: &GuestMemory, cx: &mut Context<'_>) -> Poll<(u32, Wqe)> {
        if self.head == self.tail {
            self.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }

        tracing::trace!(head = self.head, tail = self.tail, "popping wqe");

        let head = self.head;
        let mut range = self.region.range();
        range.skip((head & (self.cap - 1)) as usize);
        let mut reader = range.reader(gm);
        let header: WqeHeader = match reader.read_plain() {
            Ok(header) => header,
            Err(err) => {
                tracing::warn!(error = &err as &dyn std::error::Error, "wqe read error");
                return Poll::Pending;
            }
        };

        let total_len = header.total_len();
        if total_len > size_of::<Wqe>() || total_len > self.available() as usize {
            tracing::warn!(total_len, available = self.available(), "invalid wqe");
            return Poll::Pending;
        }

        let mut wqe = Wqe {
            header,
            data: FromZeros::new_zeroed(),
        };

        if let Err(err) = reader.read(&mut wqe.data[..wqe.header.data_len()]) {
            tracing::warn!(error = &err as &dyn std::error::Error, "wqe read error");
            return Poll::Pending;
        }

        self.head = head.wrapping_add(total_len as u32);
        Poll::Ready((head, wqe))
    }

    fn available(&self) -> u32 {
        self.tail.wrapping_sub(self.head)
    }

    fn doorbell(&mut self, val: u32) -> Option<Waker> {
        let old_len = self.available();
        assert!(old_len <= self.cap);
        let new_len = val.wrapping_sub(self.head);
        if self.head.is_multiple_of(WQE_ALIGNMENT as u32)
            && new_len > old_len
            && new_len <= self.cap
        {
            self.tail = val;
            self.waker.take()
        } else {
            None
        }
    }
}

pub struct Queues {
    pub gm: GuestMemory,
    pub driver: VmTaskDriver,
    sqs: Vec<Mutex<Option<Wq>>>,
    rqs: Vec<Mutex<Option<Wq>>>,
    cqs: Vec<Mutex<Option<Cq>>>,
    eqs: Vec<Mutex<Option<Eq>>>,
    msis: Vec<Interrupt>,
}

impl Inspect for Queues {
    fn inspect(&self, req: inspect::Request<'_>) {
        fn inspect_list<T: Inspect>(
            resp: &mut inspect::Response<'_>,
            name: &str,
            list: &[Mutex<Option<T>>],
        ) {
            resp.fields_mut(
                name,
                list.iter().enumerate().map(|(i, entry)| {
                    (
                        i + ID_OFFSET,
                        inspect::adhoc(|req| {
                            if let Some(entry) = &*entry.lock() {
                                entry.inspect(req)
                            } else {
                                req.ignore()
                            }
                        }),
                    )
                }),
            );
        }

        let mut resp = req.respond();
        inspect_list(&mut resp, "sq", &self.sqs);
        inspect_list(&mut resp, "rq", &self.rqs);
        inspect_list(&mut resp, "cq", &self.cqs);
        inspect_list(&mut resp, "eq", &self.eqs);
    }
}

#[derive(Debug, Error)]
#[error("queue {0} not found")]
pub struct QueueNotFound(u32);

impl Queues {
    pub fn new(gm: GuestMemory, driver: VmTaskDriver, msix: &MsixEmulator) -> Self {
        let msis = (0..64)
            .map(|index| msix.interrupt(index).unwrap())
            .collect();
        Self {
            gm,
            driver,
            sqs: [(); 64].map(|_| Mutex::new(None)).into(),
            rqs: [(); 64].map(|_| Mutex::new(None)).into(),
            cqs: [(); 128].map(|_| Mutex::new(None)).into(),
            eqs: [(); 64].map(|_| Mutex::new(None)).into(),
            msis,
        }
    }

    pub fn max_sqs(&self) -> u32 {
        self.sqs.len() as u32
    }

    pub fn max_rqs(&self) -> u32 {
        self.rqs.len() as u32
    }

    pub fn max_cqs(&self) -> u32 {
        self.cqs.len() as u32
    }

    pub fn max_eqs(&self) -> u32 {
        self.eqs.len() as u32
    }

    pub fn alloc_wq(&self, is_send: bool, region: DmaRegion) -> Result<u32, QueueAllocError> {
        let wqs = if is_send { &self.sqs } else { &self.rqs };
        for (i, wq) in wqs.iter().enumerate() {
            let mut wq = wq.lock();
            if wq.is_none() {
                *wq = Some(Wq::new(region)?);
                return Ok((i + ID_OFFSET) as u32);
            }
        }
        Err(QueueAllocError::NoMoreQueues)
    }

    pub fn free_wq(&self, is_send: bool, id: u32) -> Result<(), QueueNotFound> {
        let wqs = if is_send { &self.sqs } else { &self.rqs };
        wqs.get(id as usize - ID_OFFSET)
            .and_then(|q| q.lock().take())
            .ok_or(QueueNotFound(id))?;
        Ok(())
    }

    pub fn alloc_cq(&self, region: DmaRegion, eq_id: u32) -> Result<u32, QueueAllocError> {
        for (i, cq) in self.cqs.iter().enumerate() {
            let mut cq = cq.lock();
            if cq.is_none() {
                *cq = Some(Cq {
                    q: CqEq::new(region)?,
                    eq_id,
                });
                return Ok((i + ID_OFFSET) as u32);
            }
        }
        Err(QueueAllocError::NoMoreQueues)
    }

    pub fn free_cq(&self, cq_id: u32) -> Result<(), QueueNotFound> {
        self.cqs
            .get(cq_id as usize - ID_OFFSET)
            .and_then(|q| q.lock().take())
            .ok_or(QueueNotFound(cq_id))?;
        Ok(())
    }

    pub fn alloc_eq(&self, region: DmaRegion, msix: u32) -> Result<u32, QueueAllocError> {
        for (i, eq) in self.eqs.iter().enumerate() {
            let mut eq = eq.lock();
            if eq.is_none() {
                *eq = Some(Eq {
                    q: CqEq::new(region)?,
                    msix,
                });
                return Ok((i + ID_OFFSET) as u32);
            }
        }
        Err(QueueAllocError::NoMoreQueues)
    }

    pub fn update_eq_msix(&self, eq_id: u32, msix: u32) -> Result<(), QueueNotFound> {
        self.eq(eq_id)
            .map(|mut eq| {
                eq.msix = msix;
                Some(eq_id)
            })
            .ok_or(QueueNotFound(eq_id))?;
        Ok(())
    }

    pub fn free_eq(&self, eq_id: u32) -> Result<(), QueueNotFound> {
        self.eqs
            .get(eq_id as usize - ID_OFFSET)
            .and_then(|q| q.lock().take())
            .ok_or(QueueNotFound(eq_id))?;
        Ok(())
    }

    fn sq(&self, sq_id: u32) -> Option<MappedMutexGuard<'_, Wq>> {
        MutexGuard::try_map(
            self.sqs
                .get((sq_id as usize).wrapping_sub(ID_OFFSET))?
                .lock(),
            Option::as_mut,
        )
        .ok()
    }

    fn rq(&self, rq_id: u32) -> Option<MappedMutexGuard<'_, Wq>> {
        MutexGuard::try_map(
            self.rqs
                .get((rq_id as usize).wrapping_sub(ID_OFFSET))?
                .lock(),
            Option::as_mut,
        )
        .ok()
    }

    fn cq(&self, cq_id: u32) -> Option<MappedMutexGuard<'_, Cq>> {
        MutexGuard::try_map(
            self.cqs
                .get((cq_id as usize).wrapping_sub(ID_OFFSET))?
                .lock(),
            Option::as_mut,
        )
        .ok()
    }

    fn eq(&self, eq_id: u32) -> Option<MappedMutexGuard<'_, Eq>> {
        MutexGuard::try_map(
            self.eqs
                .get((eq_id as usize).wrapping_sub(ID_OFFSET))?
                .lock(),
            Option::as_mut,
        )
        .ok()
    }

    pub fn post_cq(&self, cq_id: u32, data: &[u8], wq_id: u32, is_send: bool) {
        let post_to_eq = self.cq(cq_id).and_then(|mut cq| {
            let mut cqe = Cqe {
                data: FromZeros::new_zeroed(),
                params: CqeParams::new()
                    .with_is_send_wq(is_send)
                    .with_wq_number(wq_id)
                    .with_owner_count(cq.q.owner_count()),
            };
            cqe.data[..data.len()].copy_from_slice(data);
            cq.q.post(&self.gm, &cqe).then_some(cq.eq_id)
        });

        if let Some(eq_id) = post_to_eq {
            tracing::trace!(cq_id, eq_id, "eq completion on cq post");
            self.post_eq(eq_id, GDMA_EQE_COMPLETION, &cq_id.to_ne_bytes());
        }
    }

    pub fn post_eq(&self, eq_id: u32, ty: u8, data: &[u8]) {
        let post_msi = self.eq(eq_id).and_then(|mut eq| {
            let mut eqe = Eqe {
                data: FromZeros::new_zeroed(),
                params: EqeParams::new()
                    .with_event_type(ty)
                    .with_owner_count(eq.q.owner_count()),
            };
            eqe.data[..data.len()].copy_from_slice(data);
            eq.q.post(&self.gm, &eqe).then_some(eq.msix)
        });

        if let Some(msix) = post_msi {
            tracing::trace!(eq_id, msix, "interrupt on eq post");
            self.msis[msix as usize].deliver();
        }
    }

    pub fn poll_sq(&self, sq_id: u32, cx: &mut Context<'_>) -> Poll<Wqe> {
        if let Some(mut sq) = self.sq(sq_id) {
            sq.poll_wqe(&self.gm, cx).map(|x| x.1)
        } else {
            Poll::Pending
        }
    }

    pub fn poll_rq(&self, rq_id: u32, cx: &mut Context<'_>) -> Poll<(u32, Wqe)> {
        if let Some(mut rq) = self.rq(rq_id) {
            rq.poll_wqe(&self.gm, cx)
        } else {
            Poll::Pending
        }
    }

    pub fn doorbell_sq(&self, val: WqDoorbellValue) {
        let waker = self.sq(val.id()).and_then(|mut sq| sq.doorbell(val.tail()));
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub fn doorbell_rq(&self, val: WqDoorbellValue) {
        let waker = self.rq(val.id()).and_then(|mut rq| rq.doorbell(val.tail()));
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub fn doorbell_cq(&self, val: CqEqDoorbellValue) {
        let cq_id = val.id();
        let post_to_eq = self
            .cq(cq_id)
            .and_then(|mut cq| cq.q.doorbell(val.tail(), val.arm()).then_some(cq.eq_id));

        if let Some(eq_id) = post_to_eq {
            tracing::trace!(cq_id, eq_id, "eq completion on cq doorbell");
            self.post_eq(eq_id, GDMA_EQE_COMPLETION, &cq_id.to_ne_bytes());
        }
    }

    pub fn doorbell_eq(&self, val: CqEqDoorbellValue) {
        let eq_id = val.id();
        let post_msi = self
            .eq(eq_id)
            .and_then(|mut eq| eq.q.doorbell(val.tail(), val.arm()).then_some(eq.msix));

        if let Some(msix) = post_msi {
            tracing::trace!(eq_id, msix, "interrupt on eq doorbell");
            self.msis[msix as usize].deliver();
        }
    }
}
