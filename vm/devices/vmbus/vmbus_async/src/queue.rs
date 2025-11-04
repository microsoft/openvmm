// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements the `Queue` type, which provides an abstraction over
//! a VmBus channel.

use super::core::Core;
use super::core::ReadState;
use super::core::WriteState;
use crate::core::PollError;
use futures::FutureExt;
use guestmem::AccessError;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use guestmem::ranges::PagedRange;
use inspect::Inspect;
use ring::OutgoingPacketType;
use ring::TransferPageRange;
use std::future::Future;
use std::future::poll_fn;
use std::ops::Deref;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use thiserror::Error;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::connected_async_channels;
use vmbus_ring as ring;
use vmbus_ring::FlatRingMem;
use vmbus_ring::IncomingPacketType;
use vmbus_ring::IncomingRing;
use vmbus_ring::RingMem;
use vmbus_ring::gparange::MultiPagedRangeBuf;
use zerocopy::IntoBytes;

/// A queue error.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(Box<ErrorInner>);

impl From<ErrorInner> for Error {
    fn from(value: ErrorInner) -> Self {
        Self(Box::new(value))
    }
}

impl Error {
    /// Returns true if the error is due to the channel being closed by the
    /// remote endpoint.
    pub fn is_closed_error(&self) -> bool {
        matches!(self.0.as_ref(), ErrorInner::ChannelClosed)
    }
}

#[derive(Debug, Error)]
enum ErrorInner {
    /// Failed to access guest memory.
    #[error("guest memory access error")]
    Access(#[source] AccessError),
    /// The ring buffer is corrupted.
    #[error("ring buffer error")]
    Ring(#[source] ring::Error),
    /// The channel has been closed.
    #[error("the channel has been closed")]
    ChannelClosed,
}

impl From<PollError> for ErrorInner {
    fn from(value: PollError) -> Self {
        match value {
            PollError::Ring(ring) => Self::Ring(ring),
            PollError::Closed => Self::ChannelClosed,
        }
    }
}

/// An error returned by `try_read*` methods.
#[derive(Debug, Error)]
pub enum TryReadError {
    /// The ring is empty.
    #[error("ring is empty")]
    Empty,
    /// Underlying queue error.
    #[error("queue error")]
    Queue(#[source] Error),
}

/// An error returned by `try_write*` methods.
#[derive(Debug, Error)]
pub enum TryWriteError {
    /// The ring is empty.
    #[error("ring is empty")]
    Full(usize),
    /// Underlying queue error.
    #[error("queue error")]
    Queue(#[source] Error),
}

/// An error returned by `read_external_ranges`
#[derive(Debug, Error)]
pub enum ExternalDataError {
    /// The packet is corrupted in some way (e.g. it does not specify a reasonable set of GPA ranges).
    #[error("invalid gpa ranges")]
    GpaRange(#[source] vmbus_ring::gparange::Error),

    /// The packet specifies memory that this vmbus cannot read, for some reason.
    #[error("access error")]
    Access(#[source] AccessError),

    /// Caller used `read_external_ranges` when the packet contains a buffer id,
    /// and the caller should have called `read_transfer_ranges`
    #[error("external data should have been read by calling read_transfer_ranges")]
    WrongExternalDataType,
}

/// An incoming packet batch reader.
pub struct ReadBatch<'a, M: RingMem> {
    core: &'a Core<M>,
    read: &'a mut ReadState,
}

/// The packet iterator for [`ReadBatch`].
pub struct ReadBatchIter<'a, 'b, M: RingMem>(&'a mut ReadBatch<'b, M>);

impl<'a, M: RingMem> ReadBatch<'a, M> {
    fn next_priv(&mut self) -> Result<Option<IncomingPacket<'a, M>>, Error> {
        let mut ptrs = self.read.ptrs.clone();
        match self.core.in_ring().read(&mut ptrs) {
            Ok(packet) => {
                let packet = IncomingPacket::parse(self.core.in_ring(), packet)?;
                self.read.ptrs = ptrs;
                Ok(Some(packet))
            }
            Err(ring::ReadError::Empty) => Ok(None),
            Err(ring::ReadError::Corrupt(err)) => Err(ErrorInner::Ring(err).into()),
        }
    }

    fn single_packet(mut self) -> Result<Option<PacketRef<'a, M>>, Error> {
        if let Some(packet) = self.next_priv()? {
            Ok(Some(PacketRef {
                batch: self,
                packet,
            }))
        } else {
            Ok(None)
        }
    }

    /// Returns an iterator of the packets.
    pub fn packets(&mut self) -> ReadBatchIter<'_, 'a, M> {
        ReadBatchIter(self)
    }
}

impl<'a, M: RingMem> Iterator for ReadBatchIter<'a, '_, M> {
    type Item = Result<IncomingPacket<'a, M>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next_priv().transpose()
    }
}

impl<M: RingMem> Drop for ReadBatch<'_, M> {
    fn drop(&mut self) {
        self.read.clear_poll(self.core);
        if self.core.in_ring().commit_read(&mut self.read.ptrs) {
            self.core.signal();
            self.read.signals.increment();
        }
    }
}

/// A reference to a single packet that has not been read out of the ring yet.
pub struct PacketRef<'a, M: RingMem> {
    batch: ReadBatch<'a, M>,
    packet: IncomingPacket<'a, M>,
}

impl<'a, M: RingMem> Deref for PacketRef<'a, M> {
    type Target = IncomingPacket<'a, M>;

    fn deref(&self) -> &Self::Target {
        &self.packet
    }
}

impl<'a, M: RingMem> AsRef<IncomingPacket<'a, M>> for PacketRef<'a, M> {
    fn as_ref(&self) -> &IncomingPacket<'a, M> {
        self
    }
}

impl<M: RingMem> PacketRef<'_, M> {
    /// Revert the read pointers, allowing a peek at the next packet.
    ///
    /// Use this with care: a malicious guest could change the packet's
    /// contents next time they are read. Any validation on the packet
    /// needs to be performed again next time the packet is read.
    pub fn revert(&mut self) {
        self.batch.read.ptrs.revert();
    }
}

/// An incoming packet.
pub enum IncomingPacket<'a, T: RingMem> {
    /// A data packet.
    Data(DataPacket<'a, T>),
    /// A completion packet.
    Completion(CompletionPacket<'a, T>),
}

/// An incoming data packet.
pub struct DataPacket<'a, T: RingMem> {
    ring: &'a IncomingRing<T>,
    payload: ring::RingRange,
    transaction_id: Option<u64>,
    buffer_id: Option<u16>,
    external_data: (u32, ring::RingRange),
}

impl<T: RingMem> DataPacket<'_, T> {
    /// A reader for the data payload.
    ///
    /// N.B. This reads the payload in place, so multiple instantiations of the
    /// reader may see multiple different results if the (malicious) opposite
    /// endpoint is mutating the ring buffer.
    pub fn reader(&self) -> vmbus_ring::RingRangeReader<'_, T> {
        self.payload.reader(self.ring)
    }

    /// The packet's transaction ID. Set if and only if a completion packet was
    /// requested.
    pub fn transaction_id(&self) -> Option<u64> {
        self.transaction_id
    }

    /// The number of GPA direct ranges.
    pub fn external_range_count(&self) -> usize {
        self.external_data.0 as usize
    }

    fn read_transfer_page_ranges(
        &self,
        transfer_buf: PagedRange<'_>,
        result: &mut MultiPagedRangeBuf,
    ) -> Result<(), AccessError> {
        let len = self.external_data.0 as usize;
        let mut reader = self.external_data.1.reader(self.ring);
        let available_count = reader.len() / size_of::<TransferPageRange>();
        if available_count < len {
            return Err(AccessError::OutOfRange(0, 0));
        }

        for _ in 0..len {
            let range = reader.read_plain::<TransferPageRange>()?;
            result.push_range(
                transfer_buf
                    .try_subrange(range.byte_offset as usize, range.byte_count as usize)
                    .ok_or(AccessError::OutOfRange(
                        range.byte_offset as usize,
                        range.byte_count as usize,
                    ))?,
            );
        }
        Ok(())
    }

    /// Reads the GPA direct range descriptors from the packet, extending `buf`.
    pub fn read_external_ranges(
        &self,
        buf: &mut MultiPagedRangeBuf,
    ) -> Result<(), ExternalDataError> {
        if self.buffer_id.is_some() {
            return Err(ExternalDataError::WrongExternalDataType);
        } else if self.external_data.0 == 0 {
            return Ok(());
        }

        let mut reader = self.external_data.1.reader(self.ring);
        buf.try_extend_with(reader.len() / 8, self.external_data.0 as usize, |b| {
            reader
                .read(b.as_mut_bytes())
                .map_err(ExternalDataError::Access)?;
            Ok(())
        })?
        .map_err(ExternalDataError::GpaRange)?;
        Ok(())
    }

    /// Returns the transfer buffer ID of the packet, or None if this is not a transfer packet.
    pub fn transfer_buffer_id(&self) -> Option<u16> {
        self.buffer_id
    }

    /// Reads the transfer descriptors from the packet using the provided
    /// buffer, extending `result`.
    ///
    /// This buffer should be the one associated with the value returned from
    /// transfer_buffer_id().
    pub fn read_transfer_ranges(
        &self,
        transfer_buf: PagedRange<'_>,
        result: &mut MultiPagedRangeBuf,
    ) -> Result<(), AccessError> {
        if self.external_data.0 != 0 {
            self.read_transfer_page_ranges(transfer_buf, result)?;
        }
        Ok(())
    }
}

/// A completion packet.
pub struct CompletionPacket<'a, T: RingMem> {
    ring: &'a IncomingRing<T>,
    payload: ring::RingRange,
    transaction_id: u64,
}

impl<T: RingMem> CompletionPacket<'_, T> {
    /// A reader for the completion payload.
    pub fn reader(&self) -> impl MemoryRead + '_ {
        self.payload.reader(self.ring)
    }

    /// The packet's transaction ID.
    pub fn transaction_id(&self) -> u64 {
        self.transaction_id
    }
}

impl<'a, T: RingMem> IncomingPacket<'a, T> {
    fn parse(ring: &'a IncomingRing<T>, packet: ring::IncomingPacket) -> Result<Self, Error> {
        Ok(match packet.typ {
            IncomingPacketType::InBand => IncomingPacket::Data(DataPacket {
                ring,
                payload: packet.payload,
                transaction_id: packet.transaction_id,
                buffer_id: None,
                external_data: (0, ring::RingRange::empty()),
            }),
            IncomingPacketType::GpaDirect(count, ranges) => IncomingPacket::Data(DataPacket {
                ring,
                payload: packet.payload,
                transaction_id: packet.transaction_id,
                buffer_id: None,
                external_data: (count, ranges),
            }),
            IncomingPacketType::Completion => IncomingPacket::Completion(CompletionPacket {
                ring,
                payload: packet.payload,
                transaction_id: packet.transaction_id.unwrap(),
            }),
            IncomingPacketType::TransferPages(id, count, ranges) => {
                IncomingPacket::Data(DataPacket {
                    ring,
                    payload: packet.payload,
                    transaction_id: packet.transaction_id,
                    buffer_id: Some(id),
                    external_data: (count, ranges),
                })
            }
        })
    }
}

/// The reader for the incoming ring buffer of a [`Queue`].
pub struct ReadHalf<'a, M: RingMem> {
    core: &'a Core<M>,
    read: &'a mut ReadState,
}

impl<'a, M: RingMem> ReadHalf<'a, M> {
    /// Polls the incoming ring for more packets.
    ///
    /// This will automatically manage interrupt masking. The queue will keep
    /// interrupts masked until this is called. Once this is called, interrupts
    /// will remain unmasked until this or another poll or async read function
    /// is called again.
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        ready!(self.read.poll_ready(cx, self.core)).map_err(ErrorInner::from)?;
        Poll::Ready(Ok(()))
    }

    /// Polls the incoming ring for more packets and returns a batch reader for
    /// them.
    ///
    /// This will manage interrupt masking as described in [`Self::poll_ready`].
    pub fn poll_read_batch<'b>(
        &'b mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<ReadBatch<'b, M>, Error>> {
        let batch = loop {
            std::task::ready!(self.poll_ready(cx))?;
            if self
                .core
                .in_ring()
                .can_read(&mut self.read.ptrs)
                .map_err(ErrorInner::Ring)?
            {
                break ReadBatch {
                    core: self.core,
                    read: self.read,
                };
            } else {
                self.read.clear_ready();
            }
        };
        Poll::Ready(Ok(batch))
    }

    /// Tries to get a reader for the next batch of packets.
    pub fn try_read_batch(&mut self) -> Result<ReadBatch<'_, M>, TryReadError> {
        if self
            .core
            .in_ring()
            .can_read(&mut self.read.ptrs)
            .map_err(|err| TryReadError::Queue(Error::from(ErrorInner::Ring(err))))?
        {
            Ok(ReadBatch {
                core: self.core,
                read: self.read,
            })
        } else {
            self.read.clear_ready();
            Err(TryReadError::Empty)
        }
    }

    /// Waits for the next batch of packets to be ready and returns a reader for
    /// them.
    ///
    /// This will manage interrupt masking as described in [`Self::poll_ready`].
    pub fn read_batch<'b>(&'b mut self) -> BatchRead<'a, 'b, M> {
        BatchRead(Some(self))
    }

    /// Tries to read the next packet.
    ///
    /// Returns `Err(TryReadError::Empty)` if the ring is empty.
    pub fn try_read(&mut self) -> Result<PacketRef<'_, M>, TryReadError> {
        let batch = self.try_read_batch()?;
        batch
            .single_packet()
            .map_err(TryReadError::Queue)?
            .ok_or(TryReadError::Empty)
    }

    /// Waits for the next packet to be ready and returns it.
    pub fn read<'b>(&'b mut self) -> Read<'a, 'b, M> {
        Read(self.read_batch())
    }

    /// Indicates whether pending send size notification is supported on
    /// the vmbus ring.
    pub fn supports_pending_send_size(&self) -> bool {
        self.core.in_ring().supports_pending_send_size()
    }
}

/// An asynchronous batch read operation.
pub struct BatchRead<'a, 'b, M: RingMem>(Option<&'a mut ReadHalf<'b, M>>);

impl<'a, M: RingMem> Future for BatchRead<'a, '_, M> {
    type Output = Result<ReadBatch<'a, M>, Error>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // Rebuild the batch below to get the lifetimes right.
        let _ = std::task::ready!(this.0.as_mut().unwrap().poll_read_batch(cx))?;
        let this = this.0.take().unwrap();
        Poll::Ready(Ok(ReadBatch {
            core: this.core,
            read: this.read,
        }))
    }
}

/// An asynchronous read operation.
pub struct Read<'a, 'b, M: RingMem>(BatchRead<'a, 'b, M>);

impl<'a, M: RingMem> Future for Read<'a, '_, M> {
    type Output = Result<PacketRef<'a, M>, Error>;

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let batch = std::task::ready!(self.0.poll_unpin(cx))?;
        Poll::Ready(
            batch
                .single_packet()
                .transpose()
                .expect("batch was non-empty"),
        )
    }
}

/// An outgoing packet.
pub struct OutgoingPacket<'a, 'b> {
    /// The transaction ID. Ignored for `packet_type` of [`OutgoingPacketType::InBandNoCompletion`].
    pub transaction_id: u64,
    /// The outgoing packet type.
    pub packet_type: OutgoingPacketType<'a>,
    /// The payload, as a list of byte slices.
    pub payload: &'b [&'b [u8]],
}

/// The writer for the outgoing ring buffer of a [`Queue`].
pub struct WriteHalf<'a, M: RingMem> {
    core: &'a Core<M>,
    write: &'a mut WriteState,
}

impl<'a, M: RingMem> WriteHalf<'a, M> {
    /// Polls the outgoing ring for the ability to send a packet of size
    /// `send_size`.
    ///
    /// `send_size` can be computed by calling `try_write` and extracting the
    /// size from `TryReadError::Full(send_size)`.
    pub fn poll_ready(
        &mut self,
        cx: &mut Context<'_>,
        send_size: usize,
    ) -> Poll<Result<(), Error>> {
        loop {
            std::task::ready!(self.write.poll_ready(cx, self.core, send_size))
                .map_err(ErrorInner::from)?;
            if self.can_write(send_size)? {
                break Poll::Ready(Ok(()));
            }
        }
    }

    /// Waits until there is enough space in the ring to send a packet of size
    /// `send_size`.
    ///
    /// `send_size` can be computed by calling `try_write` and extracting the
    /// size from `TryReadError::Full(send_size)`.
    pub async fn wait_ready(&mut self, send_size: usize) -> Result<(), Error> {
        poll_fn(|cx| self.poll_ready(cx, send_size)).await
    }

    /// Returns an object for writing multiple packets at once.
    ///
    /// The batch will be committed when the returned object is dropped.
    ///
    /// This reduces the overhead of writing multiple packets by updating the
    /// ring pointers and signaling an interrupt only once, when the batch is
    /// committed.
    pub fn batched(&mut self) -> WriteBatch<'_, M> {
        WriteBatch {
            core: self.core,
            write: self.write,
        }
    }

    /// Checks the outgiong ring for the capacity to send a packet of size
    /// `send_size`.
    pub fn can_write(&mut self, send_size: usize) -> Result<bool, Error> {
        self.batched().can_write(send_size)
    }

    /// The ring's capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.core.out_ring().maximum_packet_size()
    }

    /// Tries to write a packet into the outgoing ring.
    ///
    /// Fails with `TryReadError::Full(send_size)` if the ring is full.
    pub fn try_write(&mut self, packet: &OutgoingPacket<'_, '_>) -> Result<(), TryWriteError> {
        self.batched().try_write(packet)
    }

    /// Polls the ring for successful write of `packet`.
    pub fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        packet: &OutgoingPacket<'_, '_>,
    ) -> Poll<Result<(), Error>> {
        let mut send_size = 32;
        let r = loop {
            std::task::ready!(self.write.poll_ready(cx, self.core, send_size))
                .map_err(ErrorInner::from)?;
            match self.try_write(packet) {
                Ok(()) => break Ok(()),
                Err(TryWriteError::Full(len)) => send_size = len,
                Err(TryWriteError::Queue(err)) => break Err(err),
            }
        };
        Poll::Ready(r)
    }

    /// Writes a packet.
    pub fn write<'b, 'c>(&'b mut self, packet: OutgoingPacket<'c, 'b>) -> Write<'a, 'b, 'c, M> {
        Write {
            write: self,
            packet,
        }
    }
}

/// A batch writer, returned by [`WriteHalf::batched`].
pub struct WriteBatch<'a, M: RingMem> {
    core: &'a Core<M>,
    write: &'a mut WriteState,
}

impl<'a, M: RingMem> WriteBatch<'a, M> {
    /// Checks the outgiong ring for the capacity to send a packet of size
    /// `send_size`.
    pub fn can_write(&mut self, send_size: usize) -> Result<bool, Error> {
        let can_write = self
            .core
            .out_ring()
            .can_write(&mut self.write.ptrs, send_size)
            .map_err(ErrorInner::Ring)?;

        // Ensure that poll_write will check again.
        if !can_write {
            self.write.clear_ready();
        }
        Ok(can_write)
    }

    /// Tries to write a packet into the outgoing ring.
    ///
    /// Fails with `TryReadError::Full(send_size)` if the ring is full.
    pub fn try_write(&mut self, packet: &OutgoingPacket<'_, '_>) -> Result<(), TryWriteError> {
        let size = packet.payload.iter().fold(0, |a, p| a + p.len());
        let ring_packet = ring::OutgoingPacket {
            transaction_id: packet.transaction_id,
            size,
            typ: packet.packet_type,
        };
        let mut builder = self.try_start_write(&ring_packet)?;
        let mut writer = builder.writer();
        for &p in packet.payload {
            writer
                .write(p)
                .map_err(|err| TryWriteError::Queue(ErrorInner::Access(err).into()))?;
        }
        builder.finish();
        Ok(())
    }

    /// Tries to write a packet with an aligned u64 payload into the outgoing
    /// ring.
    ///
    /// This is more efficient than `try_write` when the payload is a contiguous
    /// multiple of 8 bytes.
    pub fn try_write_aligned(
        &mut self,
        transaction_id: u64,
        packet_type: OutgoingPacketType<'_>,
        data: &[u64],
    ) -> Result<(), TryWriteError> {
        let size = data.len() * 8;
        let ring_packet = ring::OutgoingPacket {
            transaction_id,
            size,
            typ: packet_type,
        };
        let mut builder = self.try_start_write(&ring_packet)?;
        builder.write_aligned_full(data);
        builder.finish();
        Ok(())
    }

    fn try_start_write(
        &mut self,
        packet: &ring::OutgoingPacket<'_>,
    ) -> Result<WritePacketBuilder<'_, 'a, M>, TryWriteError> {
        let mut ptrs = self.write.ptrs.clone();
        match self.core.out_ring().write(&mut ptrs, packet) {
            Ok(range) => Ok(WritePacketBuilder {
                batch: self,
                range,
                ptrs,
            }),
            Err(ring::WriteError::Full(n)) => {
                self.write.clear_ready();
                Err(TryWriteError::Full(n))
            }
            Err(ring::WriteError::Corrupt(err)) => {
                Err(TryWriteError::Queue(ErrorInner::Ring(err).into()))
            }
        }
    }
}

struct WritePacketBuilder<'a, 'b, M: RingMem> {
    batch: &'a mut WriteBatch<'b, M>,
    range: ring::RingRange,
    ptrs: ring::OutgoingOffset,
}

impl<M: RingMem> WritePacketBuilder<'_, '_, M> {
    fn writer(&mut self) -> vmbus_ring::RingRangeWriter<'_, M> {
        self.range.writer(self.batch.core.out_ring())
    }

    fn write_aligned_full(&mut self, data: &[u64]) {
        self.range
            .write_aligned_full(self.batch.core.out_ring(), data)
    }

    fn finish(self) {
        self.batch.write.clear_poll(self.batch.core);
        self.batch.write.ptrs = self.ptrs;
    }
}

impl<M: RingMem> Drop for WriteBatch<'_, M> {
    fn drop(&mut self) {
        if self.core.out_ring().commit_write(&mut self.write.ptrs) {
            self.core.signal();
            self.write.signals.increment();
        }
    }
}

/// An asynchronous packet write operation.
#[must_use]
pub struct Write<'a, 'b, 'c, M: RingMem> {
    write: &'b mut WriteHalf<'a, M>,
    packet: OutgoingPacket<'c, 'b>,
}

impl<M: RingMem> Future for Write<'_, '_, '_, M> {
    type Output = Result<(), Error>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.write.poll_write(cx, &this.packet)
    }
}

/// An abstraction over an open VmBus channel that provides methods to read and
/// write packets from the ring, as well as poll the ring for readiness.
///
/// This is useful when you need to operate on external data packets or send or
/// receive packets in batch. Otherwise, consider the `Channel` type.
pub struct Queue<M: RingMem> {
    core: Core<M>,
    read: ReadState,
    write: WriteState,
}

impl<M: RingMem> Inspect for Queue<M> {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .merge(&self.core)
            .field("incoming_ring", &self.read)
            .field("outgoing_ring", &self.write);
    }
}

impl<M: RingMem> Queue<M> {
    /// Constructs a `Queue` object with the given raw channel and given
    /// configuration.
    pub fn new(raw: RawAsyncChannel<M>) -> Result<Self, Error> {
        let incoming = raw.in_ring.incoming().map_err(ErrorInner::Ring)?;
        let outgoing = raw.out_ring.outgoing().map_err(ErrorInner::Ring)?;
        let core = Core::new(raw);
        let read = ReadState::new(incoming);
        let write = WriteState::new(outgoing);

        Ok(Self { core, read, write })
    }

    /// Splits the queue into a read half and write half that can be operated on
    /// independently.
    pub fn split(&mut self) -> (ReadHalf<'_, M>, WriteHalf<'_, M>) {
        (
            ReadHalf {
                core: &self.core,
                read: &mut self.read,
            },
            WriteHalf {
                core: &self.core,
                write: &mut self.write,
            },
        )
    }
}

/// Returns a pair of connected queues. Useful for testing.
pub fn connected_queues(ring_size: usize) -> (Queue<FlatRingMem>, Queue<FlatRingMem>) {
    let (host, guest) = connected_async_channels(ring_size);
    (Queue::new(host).unwrap(), Queue::new(guest).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use pal_async::timer::PolledTimer;
    use ring::OutgoingPacketType;
    use std::future::poll_fn;
    use std::time::Duration;
    use vmbus_channel::gpadl::GpadlId;
    use vmbus_channel::gpadl::GpadlMap;

    #[async_test]
    async fn test_gpa_direct() {
        use guestmem::ranges::PagedRange;

        let (mut host_queue, mut guest_queue) = connected_queues(16384);

        let gpa1: Vec<u64> = vec![4096, 8192];
        let gpa2: Vec<u64> = vec![8192];
        let gpas = vec![
            PagedRange::new(20, 4096, &gpa1).unwrap(),
            PagedRange::new(0, 200, &gpa2).unwrap(),
        ];

        let payload: &[u8] = &[0xf; 24];
        guest_queue
            .split()
            .1
            .write(OutgoingPacket {
                transaction_id: 0,
                packet_type: OutgoingPacketType::GpaDirect(&gpas),
                payload: &[payload],
            })
            .await
            .unwrap();
        host_queue
            .split()
            .0
            .read_batch()
            .await
            .unwrap()
            .packets()
            .next()
            .map(|p| match p.unwrap() {
                IncomingPacket::Data(data) => {
                    // Check the payload
                    let mut in_payload = [0_u8; 24];
                    assert_eq!(payload.len(), data.reader().len());
                    data.reader().read(&mut in_payload).unwrap();
                    assert_eq!(in_payload, payload);

                    // Check the external ranges
                    assert_eq!(data.external_range_count(), 2);
                    let mut external_data = MultiPagedRangeBuf::new();
                    data.read_external_ranges(&mut external_data).unwrap();
                    let in_gpas: Vec<PagedRange<'_>> = external_data.iter().collect();
                    assert_eq!(in_gpas.len(), gpas.len());

                    for (p, q) in in_gpas.iter().zip(gpas) {
                        assert_eq!(p.offset(), q.offset());
                        assert_eq!(p.len(), q.len());
                        assert_eq!(p.gpns(), q.gpns());
                    }
                    Ok(())
                }
                _ => Err("should be data"),
            })
            .unwrap()
            .unwrap();
    }

    #[async_test]
    async fn test_gpa_direct_empty_external_data() {
        use guestmem::ranges::PagedRange;

        let (mut host_queue, mut guest_queue) = connected_queues(16384);

        let gpa1: Vec<u64> = vec![];
        let gpas = vec![PagedRange::new(0, 0, &gpa1).unwrap()];

        let payload: &[u8] = &[0xf; 24];
        guest_queue
            .split()
            .1
            .write(OutgoingPacket {
                transaction_id: 0,
                packet_type: OutgoingPacketType::GpaDirect(&gpas),
                payload: &[payload],
            })
            .await
            .unwrap();
        host_queue
            .split()
            .0
            .read_batch()
            .await
            .unwrap()
            .packets()
            .next()
            .map(|p| match p.unwrap() {
                IncomingPacket::Data(data) => {
                    // Check the payload
                    let mut in_payload = [0_u8; 24];
                    assert_eq!(payload.len(), data.reader().len());
                    data.reader().read(&mut in_payload).unwrap();
                    assert_eq!(in_payload, payload);

                    // Check the external ranges
                    assert_eq!(data.external_range_count(), 1);
                    let mut external_data = MultiPagedRangeBuf::new();
                    let external_data_result = data.read_external_ranges(&mut external_data);
                    match external_data_result {
                        Err(ExternalDataError::GpaRange(_)) => Ok(()),
                        _ => Err("should be out of range"),
                    }
                }
                _ => Err("should be data"),
            })
            .unwrap()
            .unwrap();
    }

    #[async_test]
    async fn test_transfer_pages() {
        use guestmem::ranges::PagedRange;

        let (mut host_queue, mut guest_queue) = connected_queues(16384);

        let gpadl_map = GpadlMap::new();
        let buf = vec![0x3000_u64, 1, 2, 3];
        gpadl_map.add(
            GpadlId(13),
            MultiPagedRangeBuf::from_range_buffer(1, buf).unwrap(),
        );

        let ranges = vec![
            TransferPageRange {
                byte_count: 0x10,
                byte_offset: 0x10,
            },
            TransferPageRange {
                byte_count: 0x10,
                byte_offset: 0xfff,
            },
            TransferPageRange {
                byte_count: 0x10,
                byte_offset: 0x1000,
            },
        ];

        let payload: &[u8] = &[0xf; 24];
        guest_queue
            .split()
            .1
            .write(OutgoingPacket {
                transaction_id: 0,
                packet_type: OutgoingPacketType::TransferPages(13, &ranges),
                payload: &[payload],
            })
            .await
            .unwrap();
        host_queue
            .split()
            .0
            .read_batch()
            .await
            .unwrap()
            .packets()
            .next()
            .map(|p| match p.unwrap() {
                IncomingPacket::Data(data) => {
                    // Check the payload
                    let mut in_payload = [0_u8; 24];
                    assert_eq!(payload.len(), data.reader().len());
                    data.reader().read(&mut in_payload).unwrap();
                    assert_eq!(in_payload, payload);

                    // Check the external ranges
                    assert_eq!(data.external_range_count(), 3);
                    let gpadl_map_view = gpadl_map.view();
                    assert_eq!(data.transfer_buffer_id().unwrap(), 13);
                    let buffer_gpadl = gpadl_map_view.map(GpadlId(13)).unwrap();
                    let buffer_range = buffer_gpadl.first().unwrap();
                    let mut external_data = MultiPagedRangeBuf::new();
                    data.read_transfer_ranges(buffer_range, &mut external_data)
                        .unwrap();
                    let in_ranges: Vec<PagedRange<'_>> = external_data.iter().collect();
                    assert_eq!(in_ranges.len(), ranges.len());
                    assert_eq!(in_ranges[0].offset(), 0x10);
                    assert_eq!(in_ranges[0].len(), 0x10);
                    assert_eq!(in_ranges[0].gpns().len(), 1);
                    assert_eq!(in_ranges[0].gpns()[0], 1);

                    assert_eq!(in_ranges[1].offset(), 0xfff);
                    assert_eq!(in_ranges[1].len(), 0x10);
                    assert_eq!(in_ranges[1].gpns().len(), 2);
                    assert_eq!(in_ranges[1].gpns()[0], 1);
                    assert_eq!(in_ranges[1].gpns()[1], 2);

                    assert_eq!(in_ranges[2].offset(), 0);
                    assert_eq!(in_ranges[2].len(), 0x10);
                    assert_eq!(in_ranges[2].gpns().len(), 1);
                    assert_eq!(in_ranges[2].gpns()[0], 2);

                    Ok(())
                }
                _ => Err("should be data"),
            })
            .unwrap()
            .unwrap();
    }

    #[async_test]
    async fn test_ring_full(driver: DefaultDriver) {
        let (mut host_queue, mut guest_queue) = connected_queues(4096);

        assert!(
            poll_fn(|cx| host_queue.split().1.poll_ready(cx, 4000))
                .now_or_never()
                .is_some()
        );

        host_queue
            .split()
            .1
            .try_write(&OutgoingPacket {
                transaction_id: 0,
                packet_type: OutgoingPacketType::InBandNoCompletion,
                payload: &[&[0u8; 4000]],
            })
            .unwrap();

        let n = match host_queue
            .split()
            .1
            .try_write(&OutgoingPacket {
                transaction_id: 0,
                packet_type: OutgoingPacketType::InBandNoCompletion,
                payload: &[&[0u8; 4000]],
            })
            .unwrap_err()
        {
            TryWriteError::Full(n) => n,
            _ => unreachable!(),
        };

        let mut poll = async move {
            let mut host_queue = host_queue;
            poll_fn(|cx| host_queue.split().1.poll_ready(cx, n))
                .await
                .unwrap();
            host_queue
        }
        .boxed();

        assert!(futures::poll!(&mut poll).is_pending());
        let poll = driver.spawn("test", poll);

        PolledTimer::new(&driver)
            .sleep(Duration::from_millis(50))
            .await;

        guest_queue.split().0.read().await.unwrap();
        assert!(guest_queue.split().0.try_read().is_err());

        let mut host_queue = poll.await;

        host_queue
            .split()
            .1
            .try_write(&OutgoingPacket {
                transaction_id: 0,
                packet_type: OutgoingPacketType::InBandNoCompletion,
                payload: &[&[0u8; 4000]],
            })
            .unwrap();
    }
}
