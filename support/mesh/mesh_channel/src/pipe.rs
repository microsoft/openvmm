// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation a unidirectional byte stream pipe over mesh.

use crate::ChannelError;
use futures_io::AsyncRead;
use futures_io::AsyncWrite;
use mesh_node::local_node::HandleMessageError;
use mesh_node::local_node::HandlePortEvent;
use mesh_node::local_node::NodeError;
use mesh_node::local_node::Port;
use mesh_node::local_node::PortControl;
use mesh_node::local_node::PortField;
use mesh_node::local_node::PortWithHandler;
use mesh_node::message::Message;
use mesh_node::message::OwnedMessage;
use mesh_node::resource::Resource;
use mesh_protobuf::encoding::OptionField;
use mesh_protobuf::Protobuf;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use thiserror::Error;

/// Creates a new unidirectional pipe, returning a reader and writer.
///
/// The resulting pipe has backpressure, so that if the writer tries to write
/// too many bytes before the reader reads them, then calls to
/// `futures::AsyncWriteExt::write` will block, and calls to
/// [`AsyncWrite::poll_write`] will return [`Poll::Pending`].
pub fn pipe() -> (ReadPipe, WritePipe) {
    let (read, write) = Port::new_pair();
    let quota_bytes = 65536;
    let quota_messages = 64;
    let read = ReadPipe {
        port: read.set_handler(ReadPipeState {
            data: VecDeque::new(),
            consumed_messages: 0,
            consumed_bytes: 0,
            quota_bytes,
            closed: false,
            failed: None,
            waker: None,
        }),
        quota_messages,
        quota_bytes,
    };
    let write = WritePipe {
        port: Some(write.set_handler(WritePipeState {
            remaining_messages: quota_messages,
            remaining_bytes: quota_bytes,
            closed: false,
            failed: None,
            waker: None,
        })),
    };
    (read, write)
}

/// The read side of a pipe.
///
/// This is primarily used via [`AsyncRead`] and `futures::AsyncReadExt`.
pub struct ReadPipe {
    port: PortWithHandler<ReadPipeState>,
    quota_bytes: u32,
    quota_messages: u32,
}

struct ReadPipeState {
    data: VecDeque<u8>,
    consumed_messages: u32,
    consumed_bytes: u32,
    quota_bytes: u32,
    closed: bool,
    failed: Option<ReadError>,
    waker: Option<Waker>,
}

#[derive(Debug, Error, Clone)]
enum ReadError {
    #[error("received message beyond quota")]
    OverQuota,
    #[error("node failure")]
    NodeFailure(#[source] NodeError),
}

impl From<ReadError> for io::Error {
    fn from(err: ReadError) -> Self {
        let kind = match err {
            ReadError::OverQuota => io::ErrorKind::InvalidData,
            ReadError::NodeFailure(_) => io::ErrorKind::ConnectionReset,
        };
        io::Error::new(kind, err)
    }
}

impl AsyncRead for ReadPipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut old_waker = None;
        self.port.with_port_and_handler(|port, state| {
            if state.data.is_empty() {
                if let Some(err) = &state.failed {
                    return Err(err.clone().into()).into();
                } else if state.closed {
                    return Ok(0).into();
                }
                old_waker = state.waker.replace(cx.waker().clone());
                return Poll::Pending;
            }
            let n = state.data.len().min(buf.len());
            let (left, right) = state.data.as_slices();
            if n > left.len() {
                buf[..left.len()].copy_from_slice(left);
                buf[left.len()..n].copy_from_slice(&right[..n - left.len()]);
            } else {
                buf[..n].copy_from_slice(&left[..n]);
            }
            state.data.drain(..n);
            state.consumed_bytes += n as u32;
            if state.consumed_bytes >= self.quota_bytes / 2
                || state.consumed_messages >= self.quota_messages / 2
            {
                port.respond(Message::new(QuotaMessage {
                    bytes: state.consumed_bytes,
                    messages: state.consumed_messages,
                }));
                state.consumed_bytes = 0;
                state.consumed_messages = 0;
            }
            Ok(n).into()
        })
    }
}

impl HandlePortEvent for ReadPipeState {
    fn message(
        &mut self,
        control: &mut PortControl<'_, '_>,
        message: Message<'_>,
    ) -> Result<(), HandleMessageError> {
        if let Some(err) = &self.failed {
            return Err(HandleMessageError::new(err.clone()));
        }
        let (data, _) = message.serialize();
        if data.len() + self.data.len() + self.consumed_bytes as usize > self.quota_bytes as usize {
            self.failed = Some(ReadError::OverQuota);
            return Err(HandleMessageError::new(ReadError::OverQuota));
        }
        self.data.extend(data.as_ref());
        self.consumed_messages += 1;
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
        Ok(())
    }

    fn close(&mut self, control: &mut PortControl<'_, '_>) {
        self.closed = true;
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
    }

    fn fail(&mut self, control: &mut PortControl<'_, '_>, err: NodeError) {
        self.failed = Some(ReadError::NodeFailure(err));
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
    }

    fn drain(&mut self) -> Vec<OwnedMessage> {
        let data = std::mem::take(&mut self.data).into();
        vec![OwnedMessage::serialized(mesh_protobuf::SerializedMessage {
            data,
            resources: Vec::new(),
        })]
    }
}

/// The write side of a pipe.
///
/// This is primarily used via [`AsyncWrite`] and `futures::AsyncWriteExt`.
#[derive(Protobuf)]
#[mesh(resource = "Resource")]
pub struct WritePipe {
    #[mesh(encoding = "OptionField<PortField>")]
    port: Option<PortWithHandler<WritePipeState>>,
}

#[derive(Default)]
struct WritePipeState {
    remaining_messages: u32,
    remaining_bytes: u32,
    closed: bool,
    failed: Option<Arc<ChannelError>>,
    waker: Option<Waker>,
}

impl WritePipe {
    /// Attempts to write `buf` to the pipe without blocking. Returns the number
    /// of bytes written, an error, or [`io::ErrorKind::WouldBlock`] if the pipe
    /// is full.
    pub fn write_nonblocking(&self, buf: &[u8]) -> io::Result<usize> {
        match self.write_to_port(None, buf) {
            Poll::Ready(r) => r,
            Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
        }
    }

    fn write_to_port(&self, cx: Option<&mut Context<'_>>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let port = self.port.as_ref().ok_or(io::ErrorKind::BrokenPipe)?;
        let mut old_waker = None;
        port.with_port_and_handler(|port, state| {
            if let Some(err) = &state.failed {
                Err(io::Error::new(io::ErrorKind::ConnectionReset, err.clone())).into()
            } else if state.closed {
                Err(io::ErrorKind::BrokenPipe.into()).into()
            } else if buf.is_empty() {
                Ok(0).into()
            } else if state.remaining_messages > 0 && state.remaining_bytes > 0 {
                let n = buf.len().min(state.remaining_bytes as usize);
                state.remaining_bytes -= n as u32;
                state.remaining_messages -= 1;
                port.respond(Message::serialized(&buf[..n], Vec::new()));
                Ok(n).into()
            } else {
                if let Some(cx) = cx {
                    old_waker = state.waker.replace(cx.waker().clone());
                }
                Poll::Pending
            }
        })
    }
}

impl AsyncWrite for WritePipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.write_to_port(Some(cx), buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Ok(()).into()
    }

    fn poll_close(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.port = None;
        Ok(()).into()
    }
}

impl HandlePortEvent for WritePipeState {
    fn message(
        &mut self,
        control: &mut PortControl<'_, '_>,
        message: Message<'_>,
    ) -> Result<(), HandleMessageError> {
        if let Some(err) = &self.failed {
            return Err(HandleMessageError::new(err.clone()));
        }
        let message = message.parse::<QuotaMessage>().map_err(|err| {
            let err = Arc::new(ChannelError::from(err));
            if self.failed.is_none() {
                self.failed = Some(err.clone());
            }
            HandleMessageError::new(err)
        })?;
        if self.remaining_bytes == 0 || self.remaining_messages == 0 {
            if let Some(waker) = self.waker.take() {
                control.wake(waker);
            }
        }
        self.remaining_bytes += message.bytes;
        self.remaining_messages += message.messages;
        Ok(())
    }

    fn close(&mut self, control: &mut PortControl<'_, '_>) {
        self.closed = true;
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
    }

    fn fail(&mut self, control: &mut PortControl<'_, '_>, err: NodeError) {
        self.failed = Some(Arc::new(err.into()));
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
    }

    fn drain(&mut self) -> Vec<OwnedMessage> {
        // Send remaining quota as a message to avoid having to synchronize
        // during encoding.
        vec![OwnedMessage::new(QuotaMessage {
            bytes: self.remaining_bytes,
            messages: self.remaining_messages,
        })]
    }
}

#[derive(Protobuf)]
struct QuotaMessage {
    bytes: u32,
    messages: u32,
}

mod encoding {
    use super::ReadPipe;
    use super::ReadPipeState;
    use mesh_node::local_node::Port;
    use mesh_node::resource::Resource;
    use mesh_protobuf::encoding::MessageEncoding;
    use mesh_protobuf::inplace_none;
    use mesh_protobuf::DefaultEncoding;
    use mesh_protobuf::MessageDecode;
    use mesh_protobuf::MessageEncode;
    use mesh_protobuf::Protobuf;
    use std::collections::VecDeque;

    pub struct ReadPipeEncoder;

    impl DefaultEncoding for ReadPipe {
        type Encoding = MessageEncoding<ReadPipeEncoder>;
    }

    #[derive(Protobuf)]
    #[mesh(resource = "Resource")]
    struct SerializedReadPipe {
        port: Port,
        quota_bytes: u32,
        quota_messages: u32,
    }

    impl From<SerializedReadPipe> for ReadPipe {
        fn from(value: SerializedReadPipe) -> Self {
            let SerializedReadPipe {
                port,
                quota_bytes,
                quota_messages,
            } = value;
            Self {
                port: port.set_handler(ReadPipeState {
                    data: VecDeque::new(),
                    consumed_messages: 0,
                    consumed_bytes: 0,
                    quota_bytes,
                    closed: false,
                    failed: None,
                    waker: None,
                }),
                quota_bytes,
                quota_messages,
            }
        }
    }

    impl From<ReadPipe> for SerializedReadPipe {
        fn from(value: ReadPipe) -> Self {
            Self {
                port: value.port.remove_handler().0,
                quota_bytes: value.quota_bytes,
                quota_messages: value.quota_messages,
            }
        }
    }

    impl MessageEncode<ReadPipe, Resource> for ReadPipeEncoder {
        fn write_message(
            item: ReadPipe,
            writer: mesh_protobuf::protobuf::MessageWriter<'_, '_, Resource>,
        ) {
            <SerializedReadPipe as DefaultEncoding>::Encoding::write_message(
                SerializedReadPipe::from(item),
                writer,
            )
        }

        fn compute_message_size(
            item: &mut ReadPipe,
            mut sizer: mesh_protobuf::protobuf::MessageSizer<'_>,
        ) {
            sizer.field(1).resource();
            sizer.field(2).varint(item.quota_bytes.into());
            sizer.field(3).varint(item.quota_messages.into());
        }
    }

    impl MessageDecode<'_, ReadPipe, Resource> for ReadPipeEncoder {
        fn read_message(
            item: &mut mesh_protobuf::inplace::InplaceOption<'_, ReadPipe>,
            reader: mesh_protobuf::protobuf::MessageReader<'_, '_, Resource>,
        ) -> mesh_protobuf::Result<()> {
            inplace_none!(inner: SerializedReadPipe);
            <SerializedReadPipe as DefaultEncoding>::Encoding::read_message(&mut inner, reader)?;
            item.set(inner.take().unwrap().into());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pipe;
    use crate::pipe::ReadPipe;
    use crate::pipe::WritePipe;
    use futures::AsyncReadExt;
    use futures::AsyncWriteExt;
    use futures::FutureExt;
    use futures_concurrency::future::TryJoin;
    use mesh_node::resource::SerializedMessage;
    use pal_async::async_test;

    #[async_test]
    async fn test_pipe() {
        let (mut read, mut write) = pipe();
        let v: Vec<_> = (0..1000000).map(|x| x as u8).collect();
        let w = async {
            write.write_all(&v).await?;
            drop(write);
            Ok(())
        };
        let mut buf = Vec::new();
        let r = read.read_to_end(&mut buf);
        (r, w).try_join().await.unwrap();
        assert_eq!(buf, v);
    }

    #[async_test]
    async fn test_message_backpressure() {
        let (mut read, mut write) = pipe();
        let mut n = 0;
        while write.write(&[0]).now_or_never().is_some() {
            n += 1;
        }
        assert_eq!(n, 64);
        let mut b = [0];
        read.read(&mut b).now_or_never().unwrap().unwrap();
        write.write(&[0]).now_or_never().unwrap().unwrap();
    }

    #[async_test]
    async fn test_encoding() {
        let (read, mut write) = pipe();
        write.write_all(b"hello world").await.unwrap();
        let mut read: ReadPipe = SerializedMessage::from_message(read)
            .into_message()
            .unwrap();
        let mut write: WritePipe = SerializedMessage::from_message(write)
            .into_message()
            .unwrap();
        write.write_all(b"!").await.unwrap();
        write.close().await.unwrap();
        let mut b = Vec::new();
        read.read_to_end(&mut b).await.unwrap();
        assert_eq!(b.as_slice(), b"hello world!");
    }
}
