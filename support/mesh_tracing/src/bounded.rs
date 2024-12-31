// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded channel for sending requests to the tracing backend.
//!
//! In the future, this should be generalized and move to `mesh_channel`.

use futures::Stream;
use mesh::local_node::HandleMessageError;
use mesh::local_node::HandlePortEvent;
use mesh::local_node::NodeError;
use mesh::local_node::Port;
use mesh::local_node::PortControl;
use mesh::local_node::PortField;
use mesh::local_node::PortWithHandler;
use mesh::message::MeshField;
use mesh::payload::Protobuf;
use mesh::resource::Resource;
use mesh::Message;
use mesh::RecvError;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::marker::PhantomData;
use std::task::ready;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

pub struct BoundedReceiver<T> {
    port: PortWithHandler<ReceiverState>,
    quota: u32,
    _phantom: PhantomData<fn() -> T>,
}

/// Creates a new unidirectional bounded pipe, returning a reader and writer.
///
/// The resulting pipe has backpressure, so that if the writer tries to write
/// too many bytes before the reader reads them, then calls to
/// [`AsyncWriteExt::write`](futures::AsyncWriteExt::write) will block, and
/// calls to [`AsyncWrite::poll_write`] will return [`Poll::Pending`].
pub fn bounded<T: MeshField>(quota: u32) -> (BoundedSender<T>, BoundedReceiver<T>) {
    let (read, write) = Port::new_pair();
    let receiver = BoundedReceiver {
        port: read.set_handler(ReceiverState {
            data: VecDeque::new(),
            consumed_messages: 0,
            closed: false,
            failed: None,
            waker: None,
        }),
        quota,
        _phantom: PhantomData,
    };
    let sender = BoundedSender {
        port: write.set_handler(SenderState {
            remaining_quota: quota,
            closed: false,
            waker: None,
        }),
        _phantom: PhantomData,
    };
    (sender, receiver)
}

struct ReceiverState {
    data: VecDeque<Message>,
    consumed_messages: u32,
    closed: bool,
    failed: Option<NodeError>,
    waker: Option<Waker>,
}

impl<T: MeshField> BoundedReceiver<T> {
    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        let mut old_waker = None;
        self.port.with_port_and_handler(|port, state| {
            let Some(message) = state.data.pop_front() else {
                if let Some(err) = state.failed.take() {
                    state.closed = true;
                    return Err(RecvError::Error(err.into())).into();
                } else if state.closed {
                    return Err(RecvError::Closed).into();
                }
                old_waker = state.waker.replace(cx.waker().clone());
                return Poll::Pending;
            };
            state.consumed_messages += 1;
            if state.consumed_messages >= self.quota / 2 {
                port.respond(Message::new(QuotaMessage {
                    messages: state.consumed_messages,
                }));
                state.consumed_messages = 0;
            }
            match message.parse() {
                Ok((message,)) => Ok(message).into(),
                Err(err) => Err(RecvError::Error(err.into())).into(),
            }
        })
    }
}

impl<T: MeshField> Stream for BoundedReceiver<T> {
    type Item = T;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match ready!(self.poll_recv(cx)) {
            Ok(message) => Some(message).into(),
            Err(RecvError::Closed) => None.into(),
            Err(RecvError::Error(err)) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "bounded channel error"
                );
                None.into()
            }
        }
    }
}

impl HandlePortEvent for ReceiverState {
    fn message(
        &mut self,
        control: &mut PortControl<'_>,
        message: Message,
    ) -> Result<(), HandleMessageError> {
        if let Some(err) = &self.failed {
            return Err(HandleMessageError::new(err.clone()));
        }
        self.data.push_back(message);
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
        Ok(())
    }

    fn close(&mut self, control: &mut PortControl<'_>) {
        self.closed = true;
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
    }

    fn fail(&mut self, control: &mut PortControl<'_>, err: NodeError) {
        self.failed = Some(err);
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
    }

    fn drain(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.data).into()
    }
}

#[derive(Protobuf, Debug)]
#[mesh(resource = "Resource")]
pub struct BoundedSender<T> {
    #[mesh(encoding = "PortField")]
    port: PortWithHandler<SenderState>,
    _phantom: PhantomData<fn(T)>,
}

#[derive(Default, Debug)]
struct SenderState {
    remaining_quota: u32,
    closed: bool,
    waker: Option<Waker>,
}

#[derive(Debug)]
pub enum TrySendError {
    Full,
    Closed,
}

impl<T: MeshField> BoundedSender<T> {
    fn poll_send(&mut self, cx: &mut Context<'_>, message: &mut Option<T>) -> Poll<()> {
        let mut old_waker = None;
        self.port.with_port_and_handler(|port, state| {
            if state.closed {
                ().into()
            } else if state.remaining_quota > 0 {
                state.remaining_quota -= 1;
                port.respond(Message::new((message.take().unwrap(),)));
                ().into()
            } else {
                old_waker = state.waker.replace(cx.waker().clone());
                Poll::Pending
            }
        })
    }

    #[allow(dead_code)]
    pub async fn send(&mut self, message: T) {
        let mut message = Some(message);
        poll_fn(|cx| self.poll_send(cx, &mut message)).await
    }

    pub fn try_send(&self, message: T) -> Result<(), TrySendError> {
        self.port.with_port_and_handler(|port, state| {
            if state.closed {
                Err(TrySendError::Closed)
            } else if state.remaining_quota == 0 {
                Err(TrySendError::Full)
            } else {
                state.remaining_quota -= 1;
                port.respond(Message::new((message,)));
                Ok(())
            }
        })
    }
}

impl HandlePortEvent for SenderState {
    fn message(
        &mut self,
        control: &mut PortControl<'_>,
        message: Message,
    ) -> Result<(), HandleMessageError> {
        let message = message.parse::<QuotaMessage>().map_err(|err| {
            self.closed = true;
            HandleMessageError::new(err)
        })?;
        self.remaining_quota += message.messages;
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
        Ok(())
    }

    fn close(&mut self, control: &mut PortControl<'_>) {
        self.closed = true;
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
    }

    fn fail(&mut self, control: &mut PortControl<'_>, _err: NodeError) {
        self.closed = true;
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
    }

    fn drain(&mut self) -> Vec<Message> {
        // Send remaining quota as a message to avoid having to synchronize
        // during encoding.
        vec![Message::new(QuotaMessage {
            messages: self.remaining_quota,
        })]
    }
}

#[derive(Protobuf)]
struct QuotaMessage {
    messages: u32,
}

mod encoding {
    use super::BoundedReceiver;
    use super::ReceiverState;
    use mesh::local_node::Port;
    use mesh::message::MeshField;
    use mesh::payload::encoding::MessageEncoding;
    use mesh::payload::inplace_none;
    use mesh::payload::DefaultEncoding;
    use mesh::payload::MessageDecode;
    use mesh::payload::MessageEncode;
    use mesh::payload::Protobuf;
    use mesh::resource::Resource;
    use std::collections::VecDeque;
    use std::marker::PhantomData;

    pub struct BoundedReceiverEncoder;

    impl<T: MeshField> DefaultEncoding for BoundedReceiver<T> {
        type Encoding = MessageEncoding<BoundedReceiverEncoder>;
    }

    #[derive(Protobuf)]
    #[mesh(resource = "Resource")]
    struct SerializedBoundedReceiver {
        port: Port,
        quota: u32,
    }

    impl<T: MeshField> From<SerializedBoundedReceiver> for BoundedReceiver<T> {
        fn from(value: SerializedBoundedReceiver) -> Self {
            let SerializedBoundedReceiver { port, quota } = value;
            Self {
                port: port.set_handler(ReceiverState {
                    data: VecDeque::new(),
                    consumed_messages: 0,
                    closed: false,
                    failed: None,
                    waker: None,
                }),
                quota,
                _phantom: PhantomData,
            }
        }
    }

    impl<T: MeshField> From<BoundedReceiver<T>> for SerializedBoundedReceiver {
        fn from(value: BoundedReceiver<T>) -> Self {
            Self {
                port: value.port.remove_handler().0,
                quota: value.quota,
            }
        }
    }

    impl<T: MeshField> MessageEncode<BoundedReceiver<T>, Resource> for BoundedReceiverEncoder {
        fn write_message(
            item: BoundedReceiver<T>,
            writer: mesh::payload::protobuf::MessageWriter<'_, '_, Resource>,
        ) {
            <SerializedBoundedReceiver as DefaultEncoding>::Encoding::write_message(
                SerializedBoundedReceiver::from(item),
                writer,
            )
        }

        fn compute_message_size(
            item: &mut BoundedReceiver<T>,
            mut sizer: mesh::payload::protobuf::MessageSizer<'_>,
        ) {
            sizer.field(1).resource();
            sizer.field(2).varint(item.quota.into());
        }
    }

    impl<T: MeshField> MessageDecode<'_, BoundedReceiver<T>, Resource> for BoundedReceiverEncoder {
        fn read_message(
            item: &mut mesh::payload::inplace::InplaceOption<'_, BoundedReceiver<T>>,
            reader: mesh::payload::protobuf::MessageReader<'_, '_, Resource>,
        ) -> mesh::payload::Result<()> {
            inplace_none!(inner: SerializedBoundedReceiver);
            <SerializedBoundedReceiver as DefaultEncoding>::Encoding::read_message(
                &mut inner, reader,
            )?;
            item.set(inner.take().unwrap().into());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::bounded;
    use futures::StreamExt;
    use pal_async::async_test;

    #[async_test]
    async fn test_bounded_send_recv() {
        let (send, mut recv) = bounded::<()>(16);
        // Ensure we can send 16 times without blocking (via try_send).
        for _ in 0..16 {
            send.try_send(()).unwrap();
        }

        // Ensure we can no longer send.
        assert!(send.try_send(()).is_err());

        // Receive some messages, and then make sure we can send again, up to the expected quota.
        for _ in 0..8 {
            recv.next().await.unwrap();
        }
        for _ in 0..8 {
            send.try_send(()).unwrap();
        }
        assert!(send.try_send(()).is_err());
    }
}
