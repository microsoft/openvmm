// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asynchronous vmbus pipe channels.

use super::core::Core;
use super::core::ReadState;
use super::core::WriteState;
use crate::async_dgram::AsyncRecv;
use crate::async_dgram::AsyncSend;
use crate::core::PollError;
use futures::AsyncRead;
use futures::AsyncWrite;
use guestmem::AccessError;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::InspectMut;
use std::cmp;
use std::future::poll_fn;
use std::io;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::pin::Pin;
use std::task::ready;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;
use vmbus_channel::connected_async_channels;
use vmbus_channel::RawAsyncChannel;
use vmbus_ring as ring;
use vmbus_ring::FlatRingMem;
use vmbus_ring::RingMem;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
enum Error {
    #[error("the channel has been closed")]
    ChannelClosed,
    #[error("packet is too large for the ring")]
    PacketTooLarge,
    #[error("unexpected ring packet type")]
    UnexpectedRingPacketType,
    #[error("invalid pipe packet type {0:#x}")]
    InvalidPipePacketType(u32),
    #[error("ring buffer error")]
    Ring(#[from] ring::Error),
    #[error("memory access error")]
    Access(#[from] AccessError),
    #[error("partial packet offset is too large")]
    PartialPacketOffsetTooLarge,
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<PollError> for Error {
    fn from(value: PollError) -> Self {
        match value {
            PollError::Ring(err) => Self::Ring(err),
            PollError::Closed => Self::ChannelClosed,
        }
    }
}

impl From<Error> for io::Error {
    fn from(err: Error) -> Self {
        match err {
            Error::ChannelClosed => {
                io::Error::new(io::ErrorKind::ConnectionReset, Error::ChannelClosed)
            }
            err => io::Error::new(io::ErrorKind::Other, err),
        }
    }
}

#[derive(Debug)]
enum TryReadError {
    Empty,
    Pipe(Error),
}

impl From<ring::ReadError> for TryReadError {
    fn from(e: ring::ReadError) -> Self {
        match e {
            ring::ReadError::Empty => Self::Empty,
            ring::ReadError::Corrupt(e) => Self::Pipe(e.into()),
        }
    }
}

impl<T> From<T> for TryReadError
where
    Error: From<T>,
{
    fn from(e: T) -> Self {
        Self::Pipe(e.into())
    }
}

#[derive(Debug)]
enum TryWriteError {
    Full(usize),
    Pipe(Error),
}

impl From<ring::WriteError> for TryWriteError {
    fn from(e: ring::WriteError) -> Self {
        match e {
            ring::WriteError::Full(n) => Self::Full(n),
            ring::WriteError::Corrupt(e) => Self::Pipe(e.into()),
        }
    }
}

impl<T> From<T> for TryWriteError
where
    Error: From<T>,
{
    fn from(e: T) -> Self {
        Self::Pipe(e.into())
    }
}

impl From<TryWriteError> for io::Error {
    fn from(e: TryWriteError) -> Self {
        match e {
            TryWriteError::Full(_) => {
                io::Error::new(io::ErrorKind::WouldBlock, "the ring buffer is full")
            }
            TryWriteError::Pipe(e) => e.into(),
        }
    }
}

#[derive(Debug)]
struct PipeWriteState {
    state: WriteState,
    raw: bool,
    max_payload_len: usize,
}

impl PipeWriteState {
    fn new(ptrs: ring::OutgoingOffset, raw: bool, max_payload_len: usize) -> Self {
        Self {
            state: WriteState::new(ptrs),
            raw,
            max_payload_len,
        }
    }

    fn writer<'a, M: RingMem>(&'a mut self, core: &'a Core<M>) -> PipeWriter<'a, M> {
        PipeWriter { write: self, core }
    }
}

struct PipeWriter<'a, M: RingMem> {
    write: &'a mut PipeWriteState,
    core: &'a Core<M>,
}

impl<M: RingMem> PipeWriter<'_, M> {
    /// Tries to write a full message as a ring packet, returning
    /// Err(TryWriteError::Full(_)) if the ring is full.
    fn try_write_message(&mut self, bufs: &[IoSlice<'_>]) -> Result<usize, TryWriteError> {
        let len = bufs.iter().map(|x| x.len()).sum();
        let mut packet_len = len;
        if len > self.write.max_payload_len {
            return Err(TryWriteError::Pipe(Error::PacketTooLarge));
        }
        if !self.write.raw {
            packet_len += size_of::<ring::PipeHeader>();
        }
        let mut outgoing = self.write.state.ptrs.clone();
        let range = self.core.out_ring().write(
            &mut outgoing,
            &ring::OutgoingPacket {
                transaction_id: 0,
                size: packet_len,
                typ: ring::OutgoingPacketType::InBandNoCompletion,
            },
        )?;
        let mut writer = range.writer(self.core.out_ring());
        if !self.write.raw {
            writer.write(
                ring::PipeHeader {
                    packet_type: ring::PIPE_PACKET_TYPE_DATA,
                    len: len as u32,
                }
                .as_bytes(),
            )?;
        }
        for buf in bufs {
            writer.write(buf)?;
        }
        self.write.state.clear_poll(self.core);
        if self.core.out_ring().commit_write(&mut outgoing) {
            self.core.signal();
            self.write.state.signals.increment();
        }
        self.write.state.ptrs = outgoing;
        Ok(len)
    }

    /// Tries to write `buf` into the ring as a series of packets, possibly
    /// sending only a portion of the bytes. Returns `Ok(None)` if the ring is
    /// full.
    fn try_write_bytes(&mut self, buf: &[u8]) -> Result<usize, TryWriteError> {
        if buf.is_empty() {
            return Ok(0);
        }

        const CHUNK_SIZE: usize = 2048;
        // Write in packets of CHUNK_SIZE bytes so that the opposite endpoint can remove
        // packets as it reads data, freeing up more space for writes.
        let mut written = 0;
        let mut outgoing = self.write.state.ptrs.clone();
        for buf in buf.chunks(CHUNK_SIZE) {
            match self.core.out_ring().write(
                &mut outgoing,
                &ring::OutgoingPacket {
                    transaction_id: 0,
                    size: buf.len() + size_of::<ring::PipeHeader>(),
                    typ: ring::OutgoingPacketType::InBandNoCompletion,
                },
            ) {
                Ok(range) => {
                    let mut writer = range.writer(self.core.out_ring());
                    writer.write(
                        ring::PipeHeader {
                            packet_type: ring::PIPE_PACKET_TYPE_DATA,
                            len: buf.len() as u32,
                        }
                        .as_bytes(),
                    )?;
                    writer.write(buf)?;
                    written += buf.len();
                }
                Err(ring::WriteError::Full(n)) => {
                    if written > 0 {
                        break;
                    } else {
                        return Err(TryWriteError::Full(n));
                    }
                }
                Err(ring::WriteError::Corrupt(err)) => return Err(TryWriteError::Pipe(err.into())),
            }
        }
        assert!(written > 0);
        if self.core.out_ring().commit_write(&mut outgoing) {
            self.core.signal();
            self.write.state.signals.increment();
        }
        self.write.state.ptrs = outgoing;
        Ok(written)
    }

    /// Notifies the opposite endpoint that no more data will be written
    /// (similar to TCP's FIN). Requires ring buffer space, so may fail if this
    /// would block.
    fn try_shutdown_writes(&mut self) -> Result<(), TryWriteError> {
        if !self.write.raw {
            // Write a zero-byte message. Ignore ChannelClosed since the operation
            // has already succeeded in some sense--the opposite endpoint has
            // stopped reading data.
            match self.try_write_message(&[]) {
                Ok(_) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn poll_op<F, R>(&mut self, cx: &mut Context<'_>, mut f: F) -> Poll<Result<R, Error>>
    where
        F: FnMut(&mut Self) -> Result<R, TryWriteError>,
    {
        // Estimate the required send size. Update it later if the send actually fails.
        let mut send_size = 32;
        loop {
            std::task::ready!(self.write.state.poll_ready(cx, self.core, send_size))?;
            match f(self) {
                Ok(r) => break Poll::Ready(Ok(r)),
                Err(TryWriteError::Full(len)) => {
                    send_size = len;
                    self.write.state.clear_ready();
                }
                Err(TryWriteError::Pipe(e)) => break Poll::Ready(Err(e)),
            }
        }
    }

    fn poll_write_bytes(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Error>> {
        self.poll_op(cx, |this| this.try_write_bytes(buf))
    }

    fn poll_write_message(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, Error>> {
        self.poll_op(cx, |this| this.try_write_message(bufs))
    }

    /// Notifies the opposite endpoint that no more data will be written
    /// (similar to TCP's FIN). Requires ring buffer space, so may fail if this
    /// would block.
    fn poll_shutdown_writes(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        match self.poll_op(cx, |this| this.try_shutdown_writes()) {
            Poll::Ready(Err(Error::ChannelClosed)) => {
                // Treat a closed pipe as a successful shutdown.
                Poll::Ready(Ok(()))
            }
            r => r,
        }
    }
}

#[derive(Debug)]
struct PipeReadState {
    read: ReadState,
    max_payload_len: usize,
    raw: bool,
    eof: bool,
}

impl PipeReadState {
    fn new(ptrs: ring::IncomingOffset, raw: bool, max_payload_len: usize) -> Self {
        Self {
            read: ReadState::new(ptrs),
            raw,
            max_payload_len,
            eof: false,
        }
    }

    fn reader<'a, M: RingMem>(&'a mut self, core: &'a Core<M>) -> PipeReader<'a, M> {
        PipeReader { state: self, core }
    }
}

struct PipeReader<'a, M: RingMem> {
    state: &'a mut PipeReadState,
    core: &'a Core<M>,
}

impl<M: RingMem> PipeReader<'_, M> {
    /// Tries to read the full contents of a single message packet into `bufs`,
    /// returning `Err(TryReadError::Empty)` if the ring is empty.
    fn try_read_message(&mut self, bufs: &mut [IoSliceMut<'_>]) -> Result<usize, TryReadError> {
        let len = bufs.iter().map(|x| x.len()).sum();
        let mut incoming = self.state.read.ptrs.clone();
        match self.core.in_ring().read(&mut incoming) {
            Ok(ring::IncomingPacket {
                typ: ring::IncomingPacketType::InBand,
                payload,
                ..
            }) => {
                let mut reader = payload.reader(self.core.in_ring());
                let bytes_read = if !self.state.raw {
                    let mut header = ring::PipeHeader::new_zeroed();
                    reader.read(header.as_mut_bytes())?;
                    if header.packet_type != ring::PIPE_PACKET_TYPE_DATA {
                        return Err(TryReadError::Pipe(Error::InvalidPipePacketType(
                            header.packet_type,
                        )));
                    }
                    header.len as usize // validated by call to payload.reader.read below
                } else {
                    payload.len()
                };
                if bytes_read > cmp::min(len, self.state.max_payload_len) {
                    return Err(TryReadError::Pipe(Error::PacketTooLarge));
                }
                let mut remaining = bytes_read;
                for buf in bufs {
                    if remaining == 0 {
                        break;
                    }
                    let this_len = cmp::min(remaining, buf.len());
                    remaining -= this_len;
                    reader.read(&mut buf[..this_len])?;
                }
                self.state.read.clear_poll(self.core);
                if self.core.in_ring().commit_read(&mut incoming) {
                    self.core.signal();
                    self.state.read.signals.increment();
                }
                self.state.read.ptrs = incoming;
                Ok(bytes_read)
            }
            Ok(_) => Err(TryReadError::Pipe(Error::UnexpectedRingPacketType)),
            Err(err) => Err(err.into()),
        }
    }

    /// Tries to fill `buf` with bytes from the channel, consuming partial or
    /// full packets. Returns `Err(TryReadError::Empty)` if the ring is empty.
    fn try_read_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TryReadError> {
        if buf.is_empty() || self.state.eof {
            return Ok(0);
        }
        let mut incoming = self.state.read.ptrs.clone();
        let mut commit = incoming.clone();
        let mut total_read = 0;
        while total_read < buf.len() {
            match self.core.in_ring().read(&mut incoming) {
                Ok(ring::IncomingPacket {
                    typ: ring::IncomingPacketType::InBand,
                    payload,
                    ..
                }) => {
                    let mut reader = payload.reader(self.core.in_ring());
                    let mut header = ring::PipeHeader::new_zeroed();
                    reader.read(header.as_mut_bytes())?;
                    let (off, len) = match header.packet_type {
                        ring::PIPE_PACKET_TYPE_DATA => {
                            // A zero-byte packet indicates EOF--the opposite
                            // endpoint will not write any more data. Consume
                            // the packet only if no other data is being
                            // returned so that if the channel is saved after
                            // seeing the zero-byte packet but before a
                            // zero-byte read is returned, the EOF signal is not
                            // lost.
                            //
                            // Another solution to this would be to leave the
                            // EOF packet in the ring, but jstarks told the
                            // Linux kernel devs that they could wait for this
                            // packet to be consumed to know whether it is safe
                            // to tear down the ring buffer on the guest side.
                            if header.len == 0 {
                                if total_read == 0 {
                                    self.state.eof = true;
                                    commit = incoming.clone();
                                }
                                break;
                            }
                            (0, header.len as usize)
                        }
                        ring::PIPE_PACKET_TYPE_PARTIAL => {
                            // The read offset is stored in the high 16 bits.
                            // There should be at least one byte remaining;
                            // otherwise, the packet would have been removed.
                            let off = header.len >> 16;
                            let len = header.len & 0xffff;
                            if off >= len {
                                return Err(TryReadError::Pipe(Error::PartialPacketOffsetTooLarge));
                            }
                            (off as usize, (len - off) as usize)
                        }
                        n => return Err(TryReadError::Pipe(Error::InvalidPipePacketType(n))),
                    };
                    reader.skip(off)?;
                    let read = cmp::min(len, buf.len() - total_read);
                    reader.read(&mut buf[total_read..total_read + read])?;
                    if read < len {
                        // Update the ring with the partial packet information.
                        header.packet_type = ring::PIPE_PACKET_TYPE_PARTIAL;
                        header.len += (read as u32) << 16;
                        let mut writer = payload.writer(self.core.in_ring());
                        writer.write(header.as_bytes())?;
                    } else {
                        // The whole packet has been consumed.
                        commit = incoming.clone();
                    }
                    total_read += read;
                }
                Ok(_) => return Err(TryReadError::Pipe(Error::UnexpectedRingPacketType)),
                Err(ring::ReadError::Empty) => break,
                Err(ring::ReadError::Corrupt(err)) => return Err(err.into()),
            }
        }
        if total_read > 0 || self.state.eof {
            self.state.read.clear_poll(self.core);
            if self.core.in_ring().commit_read(&mut commit) {
                self.core.signal();
                self.state.read.signals.increment();
            }
            self.state.read.ptrs = commit;
            Ok(total_read)
        } else {
            // Need to block to get more data.
            Err(TryReadError::Empty)
        }
    }

    fn poll_op<F, R>(&mut self, cx: &mut Context<'_>, mut f: F) -> Poll<Result<R, Error>>
    where
        F: FnMut(&mut Self) -> Result<R, TryReadError>,
    {
        loop {
            std::task::ready!(self.state.read.poll_ready(cx, self.core))?;
            match f(self) {
                Ok(r) => break Poll::Ready(Ok(r)),
                Err(TryReadError::Empty) => self.state.read.clear_ready(),
                Err(TryReadError::Pipe(err)) => break Poll::Ready(Err(err)),
            }
        }
    }
    fn poll_read_bytes(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Error>> {
        self.poll_op(cx, |this| this.try_read_bytes(buf))
    }

    fn poll_read_message(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<Result<usize, Error>> {
        self.poll_op(cx, |this| this.try_read_message(bufs))
    }
}

/// An open vmbus pipe in message mode, which can send and receive datagrams.
pub struct MessagePipe<M: RingMem>(Pipe<M>);

impl<M: RingMem> InspectMut for MessagePipe<M> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.0.inspect_mut(req)
    }
}

/// An open vmbus pipe in byte mode, which can be read from and written to as a
/// byte stream.
pub struct BytePipe<M: RingMem>(Pipe<M>);

impl<M: RingMem> InspectMut for BytePipe<M> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.0.inspect_mut(req)
    }
}

/// An open pipe.
struct Pipe<M: RingMem> {
    core: Core<M>,
    read: PipeReadState,
    write: PipeWriteState,
}

impl<M: RingMem> InspectMut for Pipe<M> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .merge(&self.core)
            .field("incoming_ring", &self.read.read)
            .field("outgoing_ring", &self.write.state);
    }
}

/// The read half of a pipe.
pub struct MessageReadHalf<'a, M: RingMem> {
    core: &'a Core<M>,
    read: &'a mut PipeReadState,
}

/// The write half of a pipe.
pub struct MessageWriteHalf<'a, M: RingMem> {
    core: &'a Core<M>,
    write: &'a mut PipeWriteState,
}

/// The read half of a pipe.
pub struct ByteReadHalf<'a, M: RingMem> {
    core: &'a Core<M>,
    read: &'a mut PipeReadState,
}

/// The write half of a pipe.
pub struct ByteWriteHalf<'a, M: RingMem> {
    core: &'a Core<M>,
    write: &'a mut PipeWriteState,
}

impl<M: RingMem> MessagePipe<M> {
    /// Creates a new pipe from an open channel.
    pub fn new(channel: RawAsyncChannel<M>) -> io::Result<Self> {
        Self::new_inner(channel, false)
    }

    /// Creates a new raw pipe from an open channel.
    ///
    /// A raw pipe has no additional framing and sends and receives vmbus
    /// packets directly. As a result, packet sizes will be rounded up to an
    /// 8-byte multiple.
    pub fn new_raw(channel: RawAsyncChannel<M>) -> io::Result<Self> {
        Self::new_inner(channel, true)
    }

    fn new_inner(channel: RawAsyncChannel<M>, raw: bool) -> io::Result<Self> {
        let max_payload_len = if raw {
            // There is no inherent maximum packet size for non-pipe rings.
            // Fall back to the ring size.
            channel.out_ring.maximum_packet_size() - ring::PacketSize::in_band(0)
        } else {
            // There is a protocol-specified maximum size.
            cmp::min(
                ring::MAXIMUM_PIPE_PACKET_SIZE,
                channel.out_ring.maximum_packet_size()
                    - ring::PacketSize::in_band(size_of::<ring::PipeHeader>()),
            )
        };

        let incoming = channel.in_ring.incoming().map_err(Error::Ring)?;
        let outgoing = channel.out_ring.outgoing().map_err(Error::Ring)?;

        Ok(Self(Pipe {
            core: Core::new(channel),
            read: PipeReadState::new(incoming, raw, max_payload_len),
            write: PipeWriteState::new(outgoing, raw, max_payload_len),
        }))
    }

    /// Splits the pipe into read and write halves so that reads and writes may
    /// be concurrently issued.
    pub fn split(&mut self) -> (MessageReadHalf<'_, M>, MessageWriteHalf<'_, M>) {
        (
            MessageReadHalf {
                core: &self.0.core,
                read: &mut self.0.read,
            },
            MessageWriteHalf {
                core: &self.0.core,
                write: &mut self.0.write,
            },
        )
    }

    /// Waits for the outgoing ring buffer to have enough space to write a
    /// message of size `send_size`.
    pub async fn wait_write_ready(&mut self, send_size: usize) -> io::Result<()> {
        self.split().1.wait_ready(send_size).await
    }

    /// Tries to send a datagram, failing with [`io::ErrorKind::WouldBlock`] if
    /// there is not enough space in the ring.
    pub fn try_send(&mut self, buf: &[u8]) -> io::Result<()> {
        self.split().1.try_send(buf)
    }

    /// Tries to send a datagram, failing with [`io::ErrorKind::WouldBlock`] if
    /// there is not enough space in the ring.
    pub fn try_send_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<()> {
        self.split().1.try_send_vectored(bufs)
    }
}

impl<M: RingMem> BytePipe<M> {
    /// Creates a new pipe from an open channel.
    pub fn new(channel: RawAsyncChannel<M>) -> io::Result<Self> {
        let incoming = channel.in_ring.incoming().map_err(Error::Ring)?;
        let outgoing = channel.out_ring.outgoing().map_err(Error::Ring)?;

        Ok(Self(Pipe {
            core: Core::new(channel),
            read: PipeReadState::new(incoming, false, 0),
            write: PipeWriteState::new(outgoing, false, 0),
        }))
    }

    /// Splits the pipe into read and write halves so that reads and writes may
    /// be concurrently issued.
    pub fn split(&mut self) -> (ByteReadHalf<'_, M>, ByteWriteHalf<'_, M>) {
        (
            ByteReadHalf {
                core: &self.0.core,
                read: &mut self.0.read,
            },
            ByteWriteHalf {
                core: &self.0.core,
                write: &mut self.0.write,
            },
        )
    }
}

impl<M: RingMem + Unpin> AsyncRead for BytePipe<M> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.0
            .read
            .reader(&this.0.core)
            .poll_read_bytes(cx, buf)
            .map_err(Into::into)
    }
}

impl<M: RingMem + Unpin> AsyncRead for ByteReadHalf<'_, M> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.read
            .reader(this.core)
            .poll_read_bytes(cx, buf)
            .map_err(Into::into)
    }
}

impl<M: RingMem + Unpin> AsyncWrite for BytePipe<M> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.0
            .write
            .writer(&this.0.core)
            .poll_write_bytes(cx, buf)
            .map_err(Into::into)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.0
            .write
            .writer(&this.0.core)
            .poll_shutdown_writes(cx)
            .map_err(Into::into)
    }
}

impl<M: RingMem + Unpin> AsyncWrite for ByteWriteHalf<'_, M> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.write
            .writer(this.core)
            .poll_write_bytes(cx, buf)
            .map_err(Into::into)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.write
            .writer(this.core)
            .poll_shutdown_writes(cx)
            .map_err(Into::into)
    }
}

impl<M: RingMem> AsyncRecv for MessagePipe<M> {
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        self.0
            .read
            .reader(&self.0.core)
            .poll_read_message(cx, bufs)
            .map_err(Into::into)
    }
}

impl<M: RingMem> AsyncSend for MessagePipe<M> {
    fn poll_send(&mut self, cx: &mut Context<'_>, bufs: &[IoSlice<'_>]) -> Poll<io::Result<()>> {
        ready!(self
            .0
            .write
            .writer(&self.0.core)
            .poll_write_message(cx, bufs))?;

        Poll::Ready(Ok(()))
    }
}

impl<M: RingMem> AsyncRecv for MessageReadHalf<'_, M> {
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        self.read
            .reader(self.core)
            .poll_read_message(cx, bufs)
            .map_err(Into::into)
    }
}

impl<M: RingMem> MessageWriteHalf<'_, M> {
    /// Polls the outgoing ring for the ability to send a packet of size
    /// `send_size`.
    ///
    /// `send_size` can be computed by calling `try_write` and extracting the
    /// size from `TryReadError::Full(send_size)`.
    pub fn poll_ready(&mut self, cx: &mut Context<'_>, send_size: usize) -> Poll<io::Result<()>> {
        let send_size = if self.write.raw {
            send_size
        } else {
            send_size + size_of::<ring::PipeHeader>()
        };
        self.poll_for_ring_space(cx, ring::PacketSize::in_band(send_size))
    }

    /// Polls the outgoing ring for being completely empty, indicating that the
    /// other endpoint has read everything.
    pub fn poll_empty(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_for_ring_space(cx, self.core.out_ring().maximum_packet_size())
    }

    fn poll_for_ring_space(&mut self, cx: &mut Context<'_>, size: usize) -> Poll<io::Result<()>> {
        loop {
            std::task::ready!(self.write.state.poll_ready(cx, self.core, size))
                .map_err(Error::from)?;
            if self
                .core
                .out_ring()
                .can_write(&mut self.write.state.ptrs, size)
                .map_err(Error::from)?
            {
                break;
            }
            self.write.state.clear_ready();
        }
        Poll::Ready(Ok(()))
    }

    /// Waits until there is enough space in the ring to send a packet of size
    /// `send_size`.
    ///
    /// `send_size` can be computed by calling `try_write` and extracting the
    /// size from `TryReadError::Full(send_size)`.
    pub async fn wait_ready(&mut self, send_size: usize) -> io::Result<()> {
        poll_fn(|cx| self.poll_ready(cx, send_size)).await
    }

    /// Waits until the ring is completely empty, indicating that the other
    /// endpoint has read everything.
    pub async fn wait_empty(&mut self) -> io::Result<()> {
        poll_fn(|cx| self.poll_empty(cx)).await
    }

    /// Tries to send a datagram, failing with [`io::ErrorKind::WouldBlock`] if
    /// there is not enough space in the ring.
    pub fn try_send(&mut self, buf: &[u8]) -> io::Result<()> {
        self.write
            .writer(self.core)
            .try_write_message(&[IoSlice::new(buf)])?;
        Ok(())
    }

    /// Tries to send a datagram, failing with [`io::ErrorKind::WouldBlock`] if
    /// there is not enough space in the ring.
    pub fn try_send_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<()> {
        self.write.writer(self.core).try_write_message(bufs)?;
        Ok(())
    }
}

impl<M: RingMem> AsyncSend for MessageWriteHalf<'_, M> {
    fn poll_send(&mut self, cx: &mut Context<'_>, bufs: &[IoSlice<'_>]) -> Poll<io::Result<()>> {
        ready!(self.write.writer(self.core).poll_write_message(cx, bufs))?;

        Poll::Ready(Ok(()))
    }
}

/// Creates a pair of async connected message pipes. Useful for testing.
pub fn connected_message_pipes(
    ring_size: usize,
) -> (MessagePipe<FlatRingMem>, MessagePipe<FlatRingMem>) {
    let (host, guest) = connected_async_channels(ring_size);
    (
        MessagePipe::new(host).unwrap(),
        MessagePipe::new(guest).unwrap(),
    )
}

/// Creates a pair of async connected pipes in raw mode (with no vmbus pipe
/// framing on the packets). Useful for testing.
pub fn connected_raw_message_pipes(
    ring_size: usize,
) -> (MessagePipe<FlatRingMem>, MessagePipe<FlatRingMem>) {
    let (host, guest) = connected_async_channels(ring_size);
    (
        MessagePipe::new_raw(host).unwrap(),
        MessagePipe::new_raw(guest).unwrap(),
    )
}

/// Creates a pair of async connected byte pipes. Useful for testing.
pub fn connected_byte_pipes(ring_size: usize) -> (BytePipe<FlatRingMem>, BytePipe<FlatRingMem>) {
    let (host, guest) = connected_async_channels(ring_size);
    (BytePipe::new(host).unwrap(), BytePipe::new(guest).unwrap())
}

#[cfg(test)]
mod tests {
    use crate::async_dgram::AsyncRecvExt;
    use crate::async_dgram::AsyncSendExt;
    use crate::pipe::connected_byte_pipes;
    use crate::pipe::connected_message_pipes;
    use futures::AsyncReadExt;
    use futures::AsyncWriteExt;
    use pal_async::async_test;
    use pal_async::timer::PolledTimer;
    use pal_async::DefaultDriver;
    use std::io::ErrorKind;
    use std::time::Duration;
    use zerocopy::IntoBytes;

    #[async_test]
    async fn test_async_channel_close() {
        let (mut host, guest) = connected_message_pipes(4096);
        let mut b = [0];
        assert!(futures::poll!(host.recv(&mut b)).is_pending());
        drop(guest);
        assert_eq!(
            host.recv(&mut b).await.unwrap_err().kind(),
            ErrorKind::ConnectionReset
        );
    }

    #[async_test]
    async fn test_async_read(driver: DefaultDriver) {
        let (mut host, mut guest) = connected_message_pipes(4096);
        let guest_read = async {
            let mut b = [0; 3];
            let mut read = guest.recv(&mut b);
            assert!(futures::poll!(&mut read).is_pending());
            assert_eq!(read.await.unwrap(), 3);
            assert_eq!(&b, b"abc");
        };
        let host_write = async {
            let mut timer = PolledTimer::new(&driver);
            timer.sleep(Duration::from_millis(200)).await;
            host.send(b"abc").await.unwrap();
        };
        futures::future::join(guest_read, host_write).await;
    }

    #[async_test]
    async fn test_async_write(driver: DefaultDriver) {
        let (mut host, mut guest) = connected_message_pipes(4096);
        let v: Vec<_> = (0..2000_u16).collect();
        guest.send(v.as_bytes()).await.unwrap();
        let guest_write = async {
            let v: Vec<_> = (2000..4000_u16).collect();
            let mut write = guest.send(v.as_bytes());
            assert!(futures::poll!(&mut write).is_pending());
            write.await.unwrap();
        };
        let host_read = async {
            let mut timer = PolledTimer::new(&driver);
            timer.sleep(Duration::from_millis(200)).await;
            let mut v = [0_u16; 2000];
            let n = host.recv(v.as_mut_bytes()).await.unwrap();
            assert_eq!(n, v.as_bytes().len());
            assert!(v.iter().copied().eq(0..2000_u16));
            let n = host.recv(v.as_mut_bytes()).await.unwrap();
            assert_eq!(n, v.as_bytes().len());
            assert!(v.iter().copied().eq(2000..4000_u16));
        };
        futures::future::join(guest_write, host_read).await;
    }

    #[async_test]
    async fn test_byte_pipe(driver: DefaultDriver) {
        let (mut host, mut guest) = connected_byte_pipes(4096);
        let guest_write = async {
            let v: Vec<_> = (0..10000_u16).collect();
            let mut write = guest.write_all(v.as_bytes());
            assert!(futures::poll!(&mut write).is_pending());
            write.await.unwrap();
        };
        let host_read = async {
            let mut timer = PolledTimer::new(&driver);
            timer.sleep(Duration::from_millis(200)).await;
            let mut v = [0_u16; 10000];
            host.read_exact(v.as_mut_bytes()).await.unwrap();
            assert!(v.iter().copied().eq(0..10000_u16));
        };
        futures::future::join(guest_write, host_read).await;
    }
}
