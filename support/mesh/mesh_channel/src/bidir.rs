// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A bidirectional channel implemented on top of [`Port`].

use super::lazy::deserializer;
use super::lazy::ensure_serializable;
use super::lazy::lazy_parse;
use super::lazy::serializer;
use super::lazy::DeserializeFn;
use super::lazy::LazyMessage;
use super::lazy::SerializeFn;
use super::RecvError;
use super::TryRecvError;
use mesh_node::local_node::HandleMessageError;
use mesh_node::local_node::HandlePortEvent;
use mesh_node::local_node::NodeError;
use mesh_node::local_node::Port;
use mesh_node::local_node::PortControl;
use mesh_node::local_node::PortField;
use mesh_node::local_node::PortWithHandler;
use mesh_node::message::MeshPayload;
use mesh_node::message::Message;
use mesh_node::message::OwnedMessage;
use mesh_node::resource::SerializedMessage;
use std::any::TypeId;
use std::collections::VecDeque;
use std::fmt;
use std::fmt::Debug;
use std::future::poll_fn;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

/// One half of a bidirectional communication channel.
///
/// The port can send data of type `T` and receive data of type `U`.
///
/// This is a lower-level construct for sending and receiving binary messages.
/// Most code should use a higher-level channel returned by [`mesh::channel()`],
/// which uses this type internally.
pub struct Channel<T = SerializedMessage, U = SerializedMessage> {
    generic: GenericChannel,
    // Cached function for serializing T.
    serialize: Option<SerializeFn<T>>,
    // Cached function for deserializing U.
    deserialize: Option<DeserializeFn<U>>,
}

impl<T: MeshPayload, U: MeshPayload> mesh_protobuf::DefaultEncoding for Channel<T, U> {
    type Encoding = PortField;
}

struct GenericChannel {
    port: PortWithHandler<MessageQueue>,
    queue_drained: bool,
}

impl Debug for GenericChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenericPort")
            .field("port", &self.port)
            .field("queue_drained", &self.queue_drained)
            .finish()
    }
}

impl From<GenericChannel> for Port {
    fn from(port: GenericChannel) -> Self {
        port.port.remove_handler().0
    }
}

impl<T: 'static + MeshPayload, U: 'static + MeshPayload> From<Channel<T, U>> for Port {
    fn from(channel: Channel<T, U>) -> Self {
        channel
            .change_types::<SerializedMessage, SerializedMessage>()
            .generic
            .into()
    }
}

impl<T: 'static + MeshPayload, U: 'static + MeshPayload> From<Port> for Channel<T, U> {
    fn from(port: Port) -> Self {
        <Channel<SerializedMessage, SerializedMessage>>::new(GenericChannel::new(port))
            .change_types()
    }
}

impl<T, U> Debug for Channel<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Port")
            .field("generic", &self.generic)
            .field("serialize", &self.serialize)
            .field("deserialize", &self.deserialize)
            .finish()
    }
}

impl<T: 'static + Send, U: 'static + Send> Channel<T, U> {
    /// Creates a new bidirectional channel, returning a pair of ports.
    ///
    /// The left port can send `T` and receive `U`, and the right port can send
    /// `U` and receive `T`.
    pub fn new_pair() -> (Self, Channel<U, T>) {
        let (left, right) = GenericChannel::new_pair();
        (Self::new(left), Channel::new(right))
    }

    fn new(port: GenericChannel) -> Self {
        let serialize = (TypeId::of::<T>() == TypeId::of::<SerializedMessage>())
            .then(|| serializer::<T>().unwrap());
        let deserialize = (TypeId::of::<U>() == TypeId::of::<SerializedMessage>())
            .then(|| deserializer::<U>().unwrap());
        Self {
            generic: port,
            serialize,
            deserialize,
        }
    }
}

impl GenericChannel {
    fn new_pair() -> (Self, Self) {
        let (left, right) = Port::new_pair();
        let left = Self {
            port: left.set_handler(MessageQueue::default()),
            queue_drained: false,
        };
        let right = Self {
            port: right.set_handler(MessageQueue::default()),
            queue_drained: false,
        };
        (left, right)
    }

    fn new(port: Port) -> Self {
        Self {
            port: port.set_handler(MessageQueue::default()),
            queue_drained: false,
        }
    }

    /// Consumes and returns the first message from the incoming message queue
    /// if there are any messages available.
    fn try_recv(&self) -> Result<OwnedMessage, TryRecvError> {
        self.port.with_handler(|queue| match &queue.state {
            QueueState::Open => queue.messages.pop_front().ok_or(TryRecvError::Empty),
            QueueState::Closed => queue.messages.pop_front().ok_or(TryRecvError::Closed),
            QueueState::Failed(err) => Err(TryRecvError::Error(err.clone().into())),
        })
    }

    /// Polls the message queue.
    fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Result<OwnedMessage, RecvError>> {
        let mut old_waker = None;
        self.port.with_handler(|queue| match &queue.state {
            QueueState::Open => {
                if let Some(message) = queue.messages.pop_front() {
                    Poll::Ready(Ok(message))
                } else {
                    old_waker = queue.waker.replace(cx.waker().clone());
                    Poll::Pending
                }
            }
            QueueState::Closed => Poll::Ready(queue.messages.pop_front().ok_or(RecvError::Closed)),
            QueueState::Failed(err) => Poll::Ready(Err(RecvError::Error(err.clone().into()))),
        })
    }

    fn bridge(self, other: Self) {
        self.port
            .remove_handler()
            .0
            .bridge(other.port.remove_handler().0);
    }

    fn is_peer_closed(&self) -> bool {
        self.port.with_handler(|queue| match queue.state {
            QueueState::Open => false,
            QueueState::Closed => true,
            QueueState::Failed(_) => true,
        })
    }
}

impl<T: 'static + Send, U: 'static + Send> Channel<T, U> {
    /// Sends a message to the opposite endpoint.
    pub fn send(&self, message: T) {
        self.generic
            .port
            .send(Message::new(LazyMessage::new(message, self.serialize)))
    }

    /// Sends a message to the opposite endpoint and closes the channel in one
    /// operation.
    pub fn send_and_close(self, message: T) {
        // FUTURE: optimize by sending a single event with both message and close.
        self.generic
            .port
            .send(Message::new(LazyMessage::new(message, self.serialize)));
    }

    /// Consumes and returns the first message from the incoming message queue
    /// if there are any messages available.
    pub fn try_recv(&mut self) -> Result<U, TryRecvError> {
        self.generic
            .try_recv()?
            .try_unwrap()
            .or_else(|m| lazy_parse(m.serialize(), &mut self.deserialize))
            .map_err(|err| TryRecvError::Error(err.into()))
    }

    /// Polls the message queue.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<U, RecvError>> {
        let r = std::task::ready!(self.generic.poll_recv(cx)).and_then(|message| {
            message
                .try_unwrap()
                .or_else(|m| lazy_parse(m.serialize(), &mut self.deserialize))
                .map_err(|err| RecvError::Error(err.into()))
        });
        if r.is_err() {
            self.generic.queue_drained = true;
        }
        Poll::Ready(r)
    }

    /// Returns a future to asynchronously receive a message.
    pub fn recv(&mut self) -> impl Future<Output = Result<U, RecvError>> + Unpin + '_ {
        poll_fn(move |cx| self.poll_recv(cx))
    }

    /// Bridges two channels together so that the peer of `self` is connected
    /// directly to the peer of `other`.
    pub fn bridge(self, other: Channel<U, T>) {
        self.generic.bridge(other.generic);
    }

    /// Returns if the peer port is known to be closed (or failed).
    ///
    /// N.B. This will return true even if there is more data in the message
    ///      queue. This function is mostly useful on a sending port to know
    ///      whether there is any hope of data reaching the receive side.
    pub fn is_peer_closed(&self) -> bool {
        self.generic.is_peer_closed()
    }

    /// Returns true if the message queue is drained and the port is closed or
    /// failed.
    ///
    /// If the port has failed, this will only return true if the failure
    /// condition has been consumed.
    pub fn is_queue_drained(&self) -> bool {
        self.generic.queue_drained
    }
}

impl<T: 'static + MeshPayload, U: 'static + MeshPayload> Channel<T, U> {
    /// Changes the message types for the port.
    ///
    /// The old and new types must be serializable since the port's peer is
    /// still operating on the old types (which is intended to work fine as long
    /// as the messages have compatible serialization formats). As a result, it
    /// may be necessary to round trip messages through their serialized form.
    ///
    /// The caller must therefore ensure that the new message type is compatible
    /// with the message encoding.
    pub fn change_types<NewT: 'static + MeshPayload, NewU: 'static + MeshPayload>(
        self,
    ) -> Channel<NewT, NewU> {
        // Ensure all the types are serializable so that the peer port can
        // convert between them as necessary.
        ensure_serializable::<T>();
        ensure_serializable::<U>();
        let (serialize, _) = ensure_serializable::<NewT>();
        let (_, deserialize) = ensure_serializable::<NewU>();
        Channel {
            generic: self.generic,
            serialize: Some(serialize),
            deserialize: Some(deserialize),
        }
    }
}

#[derive(Debug, Default)]
enum QueueState {
    #[default]
    Open,
    Closed,
    Failed(NodeError),
}

#[derive(Debug, Default)]
struct MessageQueue {
    messages: VecDeque<OwnedMessage>,
    state: QueueState,
    waker: Option<Waker>,
}

impl HandlePortEvent for MessageQueue {
    fn message(
        &mut self,
        control: &mut PortControl<'_, '_>,
        message: Message<'_>,
    ) -> Result<(), HandleMessageError> {
        self.messages.push_back(message.into_owned());
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
        Ok(())
    }

    fn fail(&mut self, control: &mut PortControl<'_, '_>, err: NodeError) {
        self.state = QueueState::Failed(err);
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
    }

    fn close(&mut self, control: &mut PortControl<'_, '_>) {
        self.state = QueueState::Closed;
        if let Some(waker) = self.waker.take() {
            control.wake(waker);
        }
    }

    fn drain(&mut self) -> Vec<OwnedMessage> {
        std::mem::take(&mut self.messages).into()
    }
}
