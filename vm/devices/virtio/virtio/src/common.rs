// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::queue::QueueCoreCompleteWork;
use crate::queue::QueueCoreGetWork;
use crate::queue::QueueError;
use crate::queue::QueueParams;
use crate::queue::QueueState;
use crate::queue::QueueWork;
use crate::queue::VirtioQueuePayload;
use crate::queue::new_queue;
use crate::spec::VirtioDeviceFeatures;
use crate::spec::VirtioDeviceType;
use futures::FutureExt;
use futures::Stream;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use pal_async::wait::PolledWait;
use pal_event::Event;
use std::io::Error;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use thiserror::Error;
use vmcore::interrupt::Interrupt;

/// Read all readable payload buffers into `target`. Returns the number of bytes read.
fn read_from_payload(
    payload: &[VirtioQueuePayload],
    mem: &GuestMemory,
    target: &mut [u8],
) -> Result<usize, GuestMemoryError> {
    let mut remaining = target;
    let mut read_bytes: usize = 0;
    for payload in payload {
        if payload.writeable {
            continue;
        }
        let size = std::cmp::min(payload.length as usize, remaining.len());
        let (current, next) = remaining.split_at_mut(size);
        mem.read_at(payload.address, current)?;
        read_bytes += size;
        if next.is_empty() {
            break;
        }
        remaining = next;
    }
    Ok(read_bytes)
}

/// Total length of all readable (non-writeable) payload buffers.
fn readable_payload_length(payload: &[VirtioQueuePayload]) -> u64 {
    payload
        .iter()
        .filter(|p| !p.writeable)
        .fold(0, |acc, p| acc + p.length as u64)
}

/// Read readable payload buffers into `target`, skipping the first `offset`
/// bytes of readable data. Returns the number of bytes read.
fn read_from_payload_at_offset(
    payload: &[VirtioQueuePayload],
    offset: u64,
    mem: &GuestMemory,
    target: &mut [u8],
) -> Result<usize, GuestMemoryError> {
    let mut skip = offset;
    let mut remaining = target;
    let mut read_bytes: usize = 0;
    for payload in payload {
        if payload.writeable {
            continue;
        }
        let payload_len = payload.length as u64;
        if skip >= payload_len {
            skip -= payload_len;
            continue;
        }
        let usable = (payload_len - skip) as usize;
        let size = std::cmp::min(usable, remaining.len());
        let (current, next) = remaining.split_at_mut(size);
        // Use saturating add so that an overflowing guest-provided address
        // is guaranteed to land out of range rather than wrapping to a low
        // GPA.
        mem.read_at(payload.address.saturating_add(skip), current)?;
        read_bytes += size;
        skip = 0;
        if next.is_empty() {
            break;
        }
        remaining = next;
    }
    Ok(read_bytes)
}

/// A descriptor chain popped from a [`VirtioQueue`].
///
/// The device must call [`VirtioQueue::complete`] exactly once to post a
/// completion to the guest's used ring. Dropping without completing is a bug
/// and will not automatically post a completion.
#[must_use]
pub struct VirtioQueueCallbackWork {
    work: QueueWork,
    pub payload: Vec<VirtioQueuePayload>,
}

impl VirtioQueueCallbackWork {
    pub(crate) fn new(mut work: QueueWork) -> Self {
        let payload = std::mem::take(&mut work.payload);
        Self { work, payload }
    }

    pub fn descriptor_index(&self) -> u16 {
        self.work.descriptor_index()
    }

    // Determine the total size of all readable or all writeable payload buffers.
    pub fn get_payload_length(&self, writeable: bool) -> u64 {
        self.payload
            .iter()
            .filter(|x| x.writeable == writeable)
            .fold(0, |acc, x| acc + x.length as u64)
    }

    // Read all payload into a buffer.
    pub fn read(&self, mem: &GuestMemory, target: &mut [u8]) -> Result<usize, GuestMemoryError> {
        read_from_payload(&self.payload, mem, target)
    }

    /// Read readable payload into `target`, skipping the first `offset`
    /// bytes of readable data.
    pub fn read_at_offset(
        &self,
        offset: u64,
        mem: &GuestMemory,
        target: &mut [u8],
    ) -> Result<usize, GuestMemoryError> {
        read_from_payload_at_offset(&self.payload, offset, mem, target)
    }

    // Write the specified buffer to the payload buffers.
    pub fn write_at_offset(
        &self,
        offset: u64,
        mem: &GuestMemory,
        source: &[u8],
    ) -> Result<(), VirtioWriteError> {
        let mut skip_bytes = offset;
        let mut remaining = source;
        for payload in &self.payload {
            if !payload.writeable {
                continue;
            }

            let payload_length = payload.length as u64;
            if skip_bytes >= payload_length {
                skip_bytes -= payload_length;
                continue;
            }

            let size = std::cmp::min(
                payload_length as usize - skip_bytes as usize,
                remaining.len(),
            );
            let (current, next) = remaining.split_at(size);
            mem.write_at(payload.address + skip_bytes, current)?;
            remaining = next;
            if remaining.is_empty() {
                break;
            }
            skip_bytes = 0;
        }

        if !remaining.is_empty() {
            return Err(VirtioWriteError::NotAllWritten(source.len()));
        }

        Ok(())
    }

    pub fn write(&self, mem: &GuestMemory, source: &[u8]) -> Result<(), VirtioWriteError> {
        self.write_at_offset(0, mem, source)
    }
}

#[derive(Debug, Error)]
pub enum VirtioWriteError {
    #[error(transparent)]
    Memory(#[from] GuestMemoryError),
    #[error("{0:#x} bytes not written")]
    NotAllWritten(usize),
}

/// A descriptor that has been peeked from a [`VirtioQueue`] without advancing
/// the available index.
///
/// The descriptor remains in the available ring until [`consume`](Self::consume)
/// is called, which advances the index and returns a normal
/// [`VirtioQueueCallbackWork`] for completion.
///
/// Dropping a `PeekedWork` without consuming is a no-op — the descriptor stays
/// available for the next peek/next call.
pub struct PeekedWork<'a> {
    queue: &'a mut VirtioQueue,
    work: QueueWork,
}

impl<'a> PeekedWork<'a> {
    fn new(queue: &'a mut VirtioQueue, work: QueueWork) -> Self {
        Self { queue, work }
    }

    /// Returns the payload descriptors.
    pub fn payload(&self) -> &[VirtioQueuePayload] {
        &self.work.payload
    }

    /// Total length of all readable (guest-written) payload buffers.
    pub fn readable_length(&self) -> u64 {
        readable_payload_length(&self.work.payload)
    }

    /// Read all readable payload into `target`.
    pub fn read(&self, mem: &GuestMemory, target: &mut [u8]) -> Result<usize, GuestMemoryError> {
        read_from_payload(&self.work.payload, mem, target)
    }

    /// Read readable payload into `target`, skipping the first `offset`
    /// bytes of readable data.
    pub fn read_at_offset(
        &self,
        offset: u64,
        mem: &GuestMemory,
        target: &mut [u8],
    ) -> Result<usize, GuestMemoryError> {
        read_from_payload_at_offset(&self.work.payload, offset, mem, target)
    }

    /// Consume this peeked work, advancing the queue's available index.
    ///
    /// Returns a [`VirtioQueueCallbackWork`] that must be explicitly
    /// completed via [`VirtioQueue::complete`].
    pub fn consume(self) -> VirtioQueueCallbackWork {
        self.queue.core.advance(&self.work);
        VirtioQueueCallbackWork::new(self.work)
    }
}

#[derive(Debug, Inspect)]
pub struct VirtioQueue {
    #[inspect(flatten)]
    core: QueueCoreGetWork,
    #[inspect(flatten)]
    complete: QueueCoreCompleteWork,
    #[inspect(skip)]
    notify_guest: Interrupt,
    #[inspect(skip)]
    queue_event: PolledWait<Event>,
}

impl VirtioQueue {
    pub fn new(
        features: VirtioDeviceFeatures,
        params: QueueParams,
        mem: GuestMemory,
        notify: Interrupt,
        queue_event: PolledWait<Event>,
        initial_state: Option<QueueState>,
    ) -> Result<Self, QueueError> {
        let (get_work, complete_work) = new_queue(features, mem, params, initial_state)?;
        Ok(Self {
            core: get_work,
            complete: complete_work,
            notify_guest: notify,
            queue_event,
        })
    }

    /// Returns the current queue progress state.
    pub fn queue_state(&self) -> QueueState {
        QueueState {
            avail_index: self.core.avail_index(),
            used_index: self.complete.used_index(),
        }
    }

    /// Polls until the queue is kicked by the guest, indicating new work may be
    /// available.
    ///
    /// Before sleeping, this arms kick notification and rechecks the queue. If
    /// new data arrived during arming, it returns immediately without sleeping.
    /// On wakeup, kicks are suppressed to avoid unnecessary doorbells while
    /// the caller drains the queue.
    pub fn poll_kick(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.core.arm_for_kick() {
            ready!(self.queue_event.wait().poll_unpin(cx)).expect("waits on Event cannot fail");
        }
        Poll::Ready(())
    }

    /// Try to get the next work item from the queue. Returns `Ok(None)` if no
    /// work is currently available, or an error if there was an issue accessing
    /// the queue.
    ///
    /// This is a lightweight check that does not arm kick notification. When
    /// used in a poll loop with [`poll_kick`](Self::poll_kick), the kick will
    /// be armed automatically before sleeping.
    pub fn try_next(&mut self) -> Result<Option<VirtioQueueCallbackWork>, Error> {
        Ok(self
            .core
            .try_next_work()
            .map_err(Error::other)?
            .map(VirtioQueueCallbackWork::new))
    }

    /// Peek at the next available descriptor without advancing the available
    /// index. Returns a [`PeekedWork`] that holds the descriptor payload and
    /// a mutable reference to this queue.
    ///
    /// The descriptor stays in the available ring. Call
    /// [`PeekedWork::consume`] to advance the index and get a normal
    /// [`VirtioQueueCallbackWork`] for completion.
    ///
    /// Dropping the [`PeekedWork`] without consuming is a no-op — the
    /// descriptor remains available.
    ///
    /// Calling `try_peek` again without consuming returns the **same**
    /// descriptor (the descriptor metadata is captured at peek time), but
    /// note that the guest may have modified the underlying buffer contents
    /// in the meantime.
    pub fn try_peek(&mut self) -> Result<Option<PeekedWork<'_>>, Error> {
        let work = self.core.try_peek_work().map_err(Error::other)?;
        Ok(work.map(|w| PeekedWork::new(self, w)))
    }

    /// Waits until a descriptor is available for peeking, without advancing
    /// the available index. See [`try_peek`](Self::try_peek).
    ///
    /// Note that descriptor metadata is captured at peek time, but the guest
    /// may modify the underlying buffer contents between a peek and a
    /// subsequent consume or re-peek, so callers must not assume the buffer
    /// data is stable.
    pub async fn peek(&mut self) -> Result<PeekedWork<'_>, Error> {
        let work = loop {
            if let Some(work) = self.core.try_peek_work().map_err(Error::other)? {
                break work;
            }
            std::future::poll_fn(|cx| self.poll_kick(cx)).await;
        };
        Ok(PeekedWork::new(self, work))
    }

    /// Complete a descriptor previously obtained from this queue.
    ///
    /// Writes `bytes_written` to the used ring and delivers an interrupt
    /// to the guest (unless interrupt suppression is active).
    ///
    /// Takes ownership of the work item, ensuring it can only be completed
    /// once.
    pub fn complete(&mut self, work: VirtioQueueCallbackWork, bytes_written: u32) {
        match self.complete.complete_descriptor(&work.work, bytes_written) {
            Ok(true) => {
                self.notify_guest.deliver();
            }
            Ok(false) => {}
            Err(err) => {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to complete descriptor"
                );
            }
        }
    }

    fn poll_next_buffer(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<VirtioQueueCallbackWork, Error>> {
        loop {
            if let Some(work) = self.try_next()? {
                return Poll::Ready(Ok(work));
            }
            ready!(self.poll_kick(cx));
        }
    }
}

impl Stream for VirtioQueue {
    type Item = Result<VirtioQueueCallbackWork, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Some(ready!(self.get_mut().poll_next_buffer(cx))).into()
    }
}

pub(crate) struct VirtioDoorbells {
    registration: Option<Arc<dyn DoorbellRegistration>>,
    doorbells: Vec<Box<dyn Send + Sync>>,
}

impl VirtioDoorbells {
    pub fn new(registration: Option<Arc<dyn DoorbellRegistration>>) -> Self {
        Self {
            registration,
            doorbells: Vec::new(),
        }
    }

    pub fn add(&mut self, address: u64, value: Option<u64>, length: Option<u32>, event: &Event) {
        if let Some(registration) = &mut self.registration {
            let doorbell = registration.register_doorbell(address, value, length, event);
            if let Ok(doorbell) = doorbell {
                self.doorbells.push(doorbell);
            }
        }
    }

    pub fn clear(&mut self) {
        self.doorbells.clear();
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct DeviceTraitsSharedMemory {
    pub id: u8,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct DeviceTraits {
    pub device_id: VirtioDeviceType,
    pub device_features: VirtioDeviceFeatures,
    pub max_queues: u16,
    pub device_register_length: u32,
    pub shared_memory: DeviceTraitsSharedMemory,
}

impl Default for DeviceTraits {
    fn default() -> Self {
        Self {
            device_id: VirtioDeviceType(0),
            device_features: Default::default(),
            max_queues: 0,
            device_register_length: 0,
            shared_memory: Default::default(),
        }
    }
}

pub struct QueueResources {
    pub params: QueueParams,
    pub notify: Interrupt,
    pub event: Event,
    pub guest_memory: GuestMemory,
}
