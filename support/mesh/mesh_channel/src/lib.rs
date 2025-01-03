// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod bidir;
pub mod cancel;
pub mod cell;
mod deadline;
pub mod error;
mod lazy;
pub mod pipe;
pub mod rpc;

#[cfg(feature = "newchan")]
pub use error_newchan::*;
#[cfg(not(feature = "newchan"))]
pub use error_oldchan::*;

#[cfg(not(feature = "newchan_mpsc"))]
pub use mpsc::*;
#[cfg(feature = "newchan_mpsc")]
pub use mpsc_newchan::*;
#[cfg(not(feature = "newchan_spsc"))]
pub use spsc::*;
#[cfg(feature = "newchan_spsc")]
pub use spsc_newchan::*;

use bidir::Channel;
use mesh_node::local_node::Port;
use mesh_node::local_node::PortField;
use mesh_node::message::MeshField;
use mesh_protobuf::DefaultEncoding;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

#[cfg(feature = "newchan")]
mod error_newchan {
    pub use mesh_channel_core::ChannelError;
    pub use mesh_channel_core::ChannelErrorKind;
    pub use mesh_channel_core::RecvError;
    pub use mesh_channel_core::TryRecvError;
}

#[cfg(not(feature = "newchan"))]
mod error_oldchan {
    use thiserror::Error;

    /// An error representing a failure of a channel.
    #[derive(Debug, Error)]
    #[error(transparent)]
    pub struct ChannelError(Box<ChannelErrorInner>);

    /// The kind of channel failure.
    #[derive(Debug)]
    #[non_exhaustive]
    pub enum ChannelErrorKind {
        /// The peer node failed.
        NodeFailure,
        /// The received message contents are invalid.
        Corruption,
    }

    impl ChannelError {
        /// Returns the kind of channel failure that occurred.
        pub fn kind(&self) -> ChannelErrorKind {
            match &*self.0 {
                ChannelErrorInner::NodeFailure(_) => ChannelErrorKind::NodeFailure,
                ChannelErrorInner::Corruption(_) => ChannelErrorKind::Corruption,
            }
        }
    }

    impl From<mesh_protobuf::Error> for ChannelError {
        fn from(err: mesh_protobuf::Error) -> Self {
            Self(Box::new(ChannelErrorInner::Corruption(err)))
        }
    }

    impl From<mesh_node::local_node::NodeError> for ChannelError {
        fn from(value: mesh_node::local_node::NodeError) -> Self {
            Self(Box::new(ChannelErrorInner::NodeFailure(value)))
        }
    }

    #[derive(Debug, Error)]
    enum ChannelErrorInner {
        #[error("node failure")]
        NodeFailure(#[source] mesh_node::local_node::NodeError),
        #[error("message corruption")]
        Corruption(#[source] mesh_protobuf::Error),
    }

    #[derive(Debug, Error)]
    pub enum TryRecvError {
        #[error("channel empty")]
        Empty,
        #[error("channel closed")]
        Closed,
        #[error("channel failure")]
        Error(#[from] ChannelError),
    }

    #[derive(Debug, Error)]
    pub enum RecvError {
        #[error("channel closed")]
        Closed,
        #[error("channel failure")]
        Error(#[from] ChannelError),
    }
}

#[cfg(feature = "newchan_spsc")]
mod spsc_newchan {
    pub use mesh_channel_core::channel;
    pub use mesh_channel_core::Receiver;
    pub use mesh_channel_core::Sender;
}

#[cfg(not(feature = "newchan_spsc"))]
mod spsc {
    use crate::bidir::Channel;
    use crate::RecvError;
    use crate::TryRecvError;
    use mesh_node::local_node::Port;
    use mesh_node::local_node::PortField;
    use mesh_node::message::MeshField;
    use mesh_protobuf::DefaultEncoding;
    use std::fmt::Debug;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::Context;
    use std::task::Poll;

    /// The sending half of a channel returned by [`channel`].
    pub struct Sender<T>(Channel<(T,), ()>);

    impl<T> Debug for Sender<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            Debug::fmt(&self.0, f)
        }
    }

    impl<T: 'static + MeshField + Send> DefaultEncoding for Sender<T> {
        type Encoding = PortField;
    }

    /// The receiving half of a channel returned by [`channel`].
    pub struct Receiver<T>(Channel<(), (T,)>);

    impl<T> Debug for Receiver<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            Debug::fmt(&self.0, f)
        }
    }

    impl<T: 'static + MeshField + Send> DefaultEncoding for Receiver<T> {
        type Encoding = PortField;
    }

    impl<T: 'static + MeshField + Send> From<Port> for Sender<T> {
        fn from(port: Port) -> Self {
            Self(port.into())
        }
    }

    impl<T: 'static + MeshField + Send> From<Sender<T>> for Port {
        fn from(v: Sender<T>) -> Self {
            v.0.into()
        }
    }

    impl<T: 'static + Send> Sender<T> {
        /// Sends a message to the associated [`Receiver<T>`].
        ///
        /// Does not return a result, so messages can be silently dropped if the
        /// receiver has closed or failed. To detect such conditions, include
        /// another sender in the message you send so that the receiving thread can
        /// use it to send a response.
        ///
        /// ```rust
        /// # use mesh_channel::*;
        /// # futures::executor::block_on(async {
        /// let (send, mut recv) = channel();
        /// let (response_send, mut response_recv) = channel::<bool>();
        /// send.send((3, response_send));
        /// let (val, response_send) = recv.recv().await.unwrap();
        /// response_send.send(val == 3);
        /// assert_eq!(response_recv.recv().await.unwrap(), true);
        /// # });
        /// ```
        pub fn send(&self, msg: T) {
            self.0.send((msg,));
        }

        /// Bridges this and `recv` together, consuming both `self` and `recv`. This
        /// makes it so that anything sent to `recv` will be directly sent to this
        /// channel's peer receiver, without a separate relay step. This includes
        /// any data that was previously sent but not yet consumed.
        ///
        /// ```rust
        /// # use mesh_channel::*;
        /// let (outer_send, inner_recv) = channel::<u32>();
        /// let (inner_send, mut outer_recv) = channel::<u32>();
        ///
        /// outer_send.send(2);
        /// inner_send.send(1);
        /// inner_send.bridge(inner_recv);
        /// assert_eq!(outer_recv.try_recv().unwrap(), 1);
        /// assert_eq!(outer_recv.try_recv().unwrap(), 2);
        /// ```
        pub fn bridge(self, recv: Receiver<T>) {
            self.0.bridge(recv.0)
        }

        /// Returns whether the receiving side of the channel is known to be closed
        /// (or failed).
        ///
        /// This is useful to determine if there is any point in sending more data
        /// via this port. But even if this returns `false` messages may still fail
        /// to reach the destination.
        pub fn is_closed(&self) -> bool {
            self.0.is_peer_closed()
        }
    }

    impl<T: 'static + MeshField + Send> From<Port> for Receiver<T> {
        fn from(port: Port) -> Self {
            Self(port.into())
        }
    }

    impl<T: 'static + MeshField + Send> From<Receiver<T>> for Port {
        fn from(v: Receiver<T>) -> Self {
            v.0.into()
        }
    }

    impl<T: 'static + Send> Receiver<T> {
        /// Consumes and returns the next message, if there is one.
        ///
        /// Otherwise, returns whether the channel is empty, closed, or failed.
        ///
        /// ```rust
        /// # use mesh_channel::*;
        /// let (send, mut recv) = channel();
        /// send.send(5u32);
        /// drop(send);
        /// assert_eq!(recv.try_recv().unwrap(), 5);
        /// assert!(matches!(recv.try_recv().unwrap_err(), TryRecvError::Closed));
        /// ```
        pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
            Ok(self.0.try_recv()?.0)
        }

        /// Consumes and returns the next message, waiting until one is available.
        ///
        /// Returns immediately when the channel is closed or failed.
        ///
        /// ```rust
        /// # use mesh_channel::*;
        /// # futures::executor::block_on(async {
        /// let (send, mut recv) = channel();
        /// send.send(5u32);
        /// drop(send);
        /// assert_eq!(recv.recv().await.unwrap(), 5);
        /// assert!(matches!(recv.recv().await.unwrap_err(), RecvError::Closed));
        /// # });
        /// ```
        pub fn recv(&mut self) -> impl Future<Output = Result<T, RecvError>> + Unpin + '_ {
            // This is implemented manually instead of using an async fn to allow
            // the result to be Unpin, which is more flexible for callers.
            core::future::poll_fn(|cx| self.poll_recv(cx))
        }

        /// Polls for the next message.
        pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
            self.0.poll_recv(cx).map_ok(|x| x.0)
        }

        /// See [`Sender::bridge`].
        pub fn bridge(self, send: Sender<T>) {
            self.0.bridge(send.0)
        }
    }

    /// `Stream` implementation for a channel.
    ///
    /// Note that the output item from this throws away the distinction between the
    /// channel being closed and the channel failing due to a node error or decoding
    /// error. This simplifies most code that does not care about this distinction.
    ///
    /// If you need to distinguish between these cases, use [`Receiver::recv`] or
    /// [`Receiver::poll_recv`].
    impl<T: 'static + Send> futures_core::stream::Stream for Receiver<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(match std::task::ready!(self.0.poll_recv(cx)) {
                Ok((t,)) => Some(t),
                Err(RecvError::Closed) => None,
                Err(RecvError::Error(err)) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "channel closed due to error"
                    );
                    None
                }
            })
        }
    }

    impl<T: 'static + Send> futures_core::stream::FusedStream for Receiver<T> {
        fn is_terminated(&self) -> bool {
            self.0.is_queue_drained()
        }
    }

    /// Creates a unidirectional channel for sending objects of type `T`.
    ///
    /// Use [`Sender::send`] and [`Receiver::recv`] to communicate between the ends
    /// of the channel.
    ///
    /// Both channel endpoints are initially local to this process, but either or
    /// both endpoints may be sent to other processes via a cross-process channel
    /// that has already been established.
    ///
    /// ```rust
    /// # use mesh_channel::*;
    /// # futures::executor::block_on(async {
    /// let (send, mut recv) = channel::<u32>();
    /// send.send(5);
    /// let n = recv.recv().await.unwrap();
    /// assert_eq!(n, 5);
    /// # });
    /// ```
    pub fn channel<T: 'static + Send>() -> (Sender<T>, Receiver<T>) {
        let (left, right) = Channel::new_pair();
        (Sender(left), Receiver(right))
    }
}

/// The sending half of a channel returned by [`oneshot`].
pub struct OneshotSender<T>(Channel<(T,), ()>);

impl<T> Debug for OneshotSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl<T: 'static + MeshField + Send> DefaultEncoding for OneshotSender<T> {
    type Encoding = PortField;
}

impl<T: 'static + MeshField + Send> From<Port> for OneshotSender<T> {
    fn from(port: Port) -> Self {
        Self(port.into())
    }
}

impl<T: 'static + MeshField + Send> From<OneshotSender<T>> for Port {
    fn from(v: OneshotSender<T>) -> Self {
        v.0.into()
    }
}

impl<T: 'static + Send> OneshotSender<T> {
    /// Sends `value` to the receiving endpoint of the channel.
    pub fn send(self, value: T) {
        self.0.send_and_close((value,));
    }
}

/// The receiving half of a channel returned by [`oneshot`].
///
/// A value is received by `poll`ing or `await`ing the channel.
pub struct OneshotReceiver<T>(Channel<(), (T,)>);

impl<T> Debug for OneshotReceiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl<T: 'static + MeshField + Send> DefaultEncoding for OneshotReceiver<T> {
    type Encoding = PortField;
}

impl<T: 'static + Send> Future for OneshotReceiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let (message,) = std::task::ready!(self.0.poll_recv(cx))?;
        Poll::Ready(Ok(message))
    }
}

impl<T: 'static + MeshField + Send> From<Port> for OneshotReceiver<T> {
    fn from(port: Port) -> Self {
        Self(port.into())
    }
}

impl<T: 'static + MeshField + Send> From<OneshotReceiver<T>> for Port {
    fn from(v: OneshotReceiver<T>) -> Self {
        v.0.into()
    }
}

/// Creates a unidirection channel for sending a single value of type `T`.
///
/// The channel is automatically closed after the value is sent. Use this
/// instead of [`channel`] when only one value ever needs to be sent to avoid
/// programming errors where the channel is left open longer than necessary.
/// This is also more efficient.
///
/// Use [`OneshotSender::send`] and [`OneshotReceiver`] (directly as a future)
/// to communicate between the ends of the channel.
///
/// `T` must implement [`MeshField`]. Most typically this is done by
/// deriving [`MeshPayload`](mesh_node::message::MeshPayload).
///
/// Both channel endpoints are initially local to this process, but either or
/// both endpoints may be sent to other processes via a cross-process channel
/// that has already been established.
///
/// ```rust
/// # use mesh_channel::*;
/// # futures::executor::block_on(async {
/// let (send, recv) = oneshot::<u32>();
/// send.send(5);
/// let n = recv.await.unwrap();
/// assert_eq!(n, 5);
/// # });
/// ```
pub fn oneshot<T: 'static + Send>() -> (OneshotSender<T>, OneshotReceiver<T>) {
    let (left, right) = Channel::new_pair();
    (OneshotSender(left), OneshotReceiver(right))
}

#[cfg(feature = "newchan_mpsc")]
mod mpsc_newchan {
    pub use mesh_channel_core::channel as mpsc_channel;
    pub use mesh_channel_core::Receiver as MpscReceiver;
    pub use mesh_channel_core::Sender as MpscSender;
}

#[cfg(not(feature = "newchan_mpsc"))]
mod mpsc {
    use crate::bidir::Channel;
    use crate::RecvError;
    use mesh_node::message::MeshField;
    use mesh_protobuf::Protobuf;
    use std::fmt::Debug;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::Context;
    use std::task::Poll;

    /// Creates a multi-producer, single-consumer channel for sending objects of
    /// type `T`.
    ///
    /// The main difference between these channels and those returned by [`channel`]
    /// is that the sender can be cloned and sent to remote processes. This is
    /// useful when you are collating data from multiple sources.
    ///
    /// # Performance
    ///
    /// Care must be taken to avoid scaling problems with this type. Internally this
    /// uses multiple ports between the receiving end and the sending ends, and
    /// receiving is linear in the number of ports.
    ///
    /// An ordinary call to `clone` won't allocate a new port, nor will sending a
    /// clone within a process. But sending a clone to a different process will
    /// allocate a new port.
    pub fn mpsc_channel<T: 'static + Send>() -> (MpscSender<T>, MpscReceiver<T>) {
        let (send, recv) = Channel::new_pair();
        (
            MpscSender(Arc::new(MpscSenderInner(send))),
            MpscReceiver {
                receivers: vec![recv],
            },
        )
    }

    #[derive(Debug, Protobuf)]
    #[mesh(
        bound = "T: 'static + MeshField + Send",
        resource = "mesh_node::resource::Resource"
    )]
    enum MpscMessage<T> {
        Data(T),
        Clone(Channel<(), MpscMessage<T>>),
    }

    /// Receiver type for [`mpsc_channel()`].
    #[derive(Protobuf)]
    #[mesh(
        bound = "T: 'static + MeshField + Send",
        resource = "mesh_node::resource::Resource"
    )]
    pub struct MpscReceiver<T> {
        receivers: Vec<Channel<(), MpscMessage<T>>>,
    }

    impl<T> Debug for MpscReceiver<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MpscReceiver")
                .field("receivers", &self.receivers)
                .finish()
        }
    }

    impl<T: 'static + Send> MpscReceiver<T> {
        /// Creates a new receiver with no senders.
        ///
        /// Receives will fail with [`RecvError::Closed`] until [`Self::sender`] is
        /// called.
        pub fn new() -> Self {
            MpscReceiver {
                receivers: Vec::new(),
            }
        }

        /// Creates a new sender for sending data to this receiver.
        ///
        /// Note that this may transition the channel from the closed to open state.
        pub fn sender(&mut self) -> MpscSender<T> {
            let (send, recv) = Channel::new_pair();
            self.receivers.push(recv);
            MpscSender(Arc::new(MpscSenderInner(send)))
        }

        /// Consumes and returns the next message, waiting until one is available.
        ///
        /// Returns immediately when the channel is closed or failed.
        ///
        /// ```rust
        /// # use mesh_channel::*;
        /// # futures::executor::block_on(async {
        /// let (send, mut recv) = mpsc_channel();
        /// send.send(5u32);
        /// drop(send);
        /// assert_eq!(recv.recv().await.unwrap(), 5);
        /// assert!(matches!(recv.recv().await.unwrap_err(), RecvError::Closed));
        /// # });
        /// ```
        pub fn recv(&mut self) -> impl Future<Output = Result<T, RecvError>> + '_ {
            std::future::poll_fn(move |cx| self.poll_recv(cx))
        }

        fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
            let receivers = &mut self.receivers;
            let mut i = 0;
            while i < receivers.len() {
                let recv = &mut receivers[i];
                match recv.poll_recv(cx) {
                    Poll::Ready(Ok(message)) => match message {
                        MpscMessage::Data(inner_message) => {
                            return Poll::Ready(Ok(inner_message));
                        }
                        MpscMessage::Clone(new_recv) => {
                            receivers.push(new_recv);
                        }
                    },
                    Poll::Ready(Err(_)) => {
                        receivers.swap_remove(i);
                    }
                    Poll::Pending => {
                        i += 1;
                    }
                }
            }
            if receivers.is_empty() {
                Poll::Ready(Err(RecvError::Closed))
            } else {
                Poll::Pending
            }
        }
    }

    impl<T: 'static + Send> Default for MpscReceiver<T> {
        fn default() -> Self {
            Self::new()
        }
    }

    impl<T: 'static + Send> futures_core::stream::FusedStream for MpscReceiver<T> {
        fn is_terminated(&self) -> bool {
            self.receivers.is_empty()
        }
    }

    impl<T: 'static + Send> futures_core::stream::Stream for MpscReceiver<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(std::task::ready!(self.poll_recv(cx)).ok())
        }
    }

    /// Sender type for [`mpsc_channel()`].
    //
    // This wraps the actual sender in an Arc to ensure that clones within the same
    // process are cheap. When this is encoded for sending to a remote process, only
    // then will the receiver be notified of a new mesh port.
    #[derive(Protobuf)]
    #[mesh(
        bound = "T: 'static + MeshField + Send",
        resource = "mesh_node::resource::Resource"
    )]
    pub struct MpscSender<T>(Arc<MpscSenderInner<T>>);

    impl<T> Debug for MpscSender<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            Debug::fmt(&self.0 .0, f)
        }
    }

    // Manual implementation since T might not be Clone.
    impl<T> Clone for MpscSender<T> {
        fn clone(&self) -> Self {
            Self(self.0.clone())
        }
    }

    /// Wrapper that implements Clone.
    #[derive(Protobuf)]
    #[mesh(
        bound = "T: 'static + MeshField + Send",
        resource = "mesh_node::resource::Resource"
    )]
    struct MpscSenderInner<T>(Channel<MpscMessage<T>, ()>);

    impl<T: 'static + Send> Clone for MpscSenderInner<T> {
        fn clone(&self) -> Self {
            // Clone the sender by sending a new port to the receiver.
            let (send, recv) = Channel::new_pair();
            self.0.send(MpscMessage::Clone(recv));
            Self(send)
        }
    }

    impl<T: 'static + Send> MpscSender<T> {
        /// Sends a message to the receiver.
        pub fn send(&self, msg: T) {
            (self.0).0.send(MpscMessage::Data(msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_node::message::MeshPayload;
    use mesh_protobuf::SerializedMessage;
    use pal_async::async_test;
    use pal_event::Event;
    use test_with_tracing::test;

    #[test]
    fn test() {
        let (send, mut recv) = channel::<(String, String)>();
        send.send(("abc".to_string(), "def".to_string()));
        assert_eq!(
            recv.try_recv().unwrap(),
            ("abc".to_string(), "def".to_string())
        );
    }

    #[test]
    fn test_send_port() {
        let (send, mut recv) = channel::<Receiver<u32>>();
        let (sendi, recvi) = channel::<u32>();
        send.send(recvi);
        let mut recvi = recv.try_recv().unwrap();
        sendi.send(0xf00d);
        assert_eq!(recvi.try_recv().unwrap(), 0xf00d);
    }

    #[test]
    fn test_send_resource() {
        let (send, mut recv) = channel::<Event>();
        let event = Event::new();
        send.send(event.clone());
        let event2 = recv.try_recv().unwrap();
        event2.signal();
        event.wait();
    }

    #[async_test]
    async fn test_oneshot() {
        let (send, mut recv) = oneshot::<u32>();
        send.send(5);
        recv.0.recv().await.unwrap();
        assert!(matches!(
            recv.0.recv().await.unwrap_err(),
            RecvError::Closed
        ));
    }

    #[async_test]
    async fn test_mpsc() {
        let (send, mut recv) = mpsc_channel::<u32>();
        send.send(5);
        roundtrip((send.clone(),)).0.send(6);
        drop(send);
        let a = recv.recv().await.unwrap();
        let b = recv.recv().await.unwrap();
        assert!(matches!(recv.recv().await.unwrap_err(), RecvError::Closed));
        let mut s = [a, b];
        s.sort_unstable();
        assert_eq!(&s, &[5, 6]);
    }

    #[async_test]
    async fn test_mpsc_again() {
        let (send, recv) = mpsc_channel::<u32>();
        drop(recv);
        send.send(5);
    }

    /// Serializes and deserializes a mesh message. Used to force an MpscSender
    /// to clone its underlying port.
    fn roundtrip<T: MeshPayload>(t: T) -> T {
        SerializedMessage::from_message(t).into_message().unwrap()
    }
}
