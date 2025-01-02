// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of a multi-producer, single-consumer (MPSC) channel that can
//! be used to communicate between mesh nodes.

// UNSAFETY: Needed to erase types to avoid monomorphization overhead.
#![expect(unsafe_code)]

use crate::deque::ElementVtable;
use crate::deque::ErasedVecDeque;
use crate::error::ChannelError;
use crate::error::RecvError;
use crate::error::TryRecvError;
use core::fmt::Debug;
use core::future::Future;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::mem::MaybeUninit;
use core::task::Context;
use core::task::Poll;
use core::task::Waker;
use mesh_node::local_node::HandleMessageError;
use mesh_node::local_node::HandlePortEvent;
use mesh_node::local_node::Port;
use mesh_node::local_node::PortField;
use mesh_node::local_node::PortWithHandler;
use mesh_node::message::MeshField;
use mesh_node::message::Message;
use mesh_protobuf::DefaultEncoding;
use mesh_protobuf::Protobuf;
use parking_lot::Mutex;
use parking_lot::MutexGuard;
use std::sync::Arc;
use std::sync::OnceLock;

/// Creates a new channel for sending messages of type `T`, returning the sender
/// and receiver ends.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    fn channel_core(vtable: &'static ElementVtable) -> (SenderCore, ReceiverCore) {
        let mut receiver = ReceiverCore::new(vtable);
        let sender = receiver.sender();
        (sender, receiver)
    }
    let (sender, receiver) = channel_core(const { &ElementVtable::new::<T>() });
    (Sender(sender, PhantomData), Receiver(receiver, PhantomData))
}

/// The sending half of a channel returned by [`channel`].
///
/// The sender can be cloned to send messages from multiple threads or
/// processes.
//
// Note that the `PhantomData` here is necessary to ensure `Send/Sync` traits
// are only implemented when `T` is `Send`, since the `SenderCore` is always
// `Send+Sync`. This behavior is verified in the unit tests.
pub struct Sender<T>(SenderCore, PhantomData<Arc<Mutex<[T]>>>);

impl<T> Debug for Sender<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), PhantomData)
    }
}

impl<T> Sender<T> {
    /// Sends a message to the associated [`Receiver<T>`].
    ///
    /// Does not return a result, so messages can be silently dropped if the
    /// receiver has closed or failed. To detect such conditions, include
    /// another sender in the message you send so that the receiving thread can
    /// use it to send a response.
    ///
    /// ```rust
    /// # use mesh_channel_core::*;
    /// # futures::executor::block_on(async {
    /// let (send, mut recv) = channel();
    /// let (response_send, mut response_recv) = channel::<bool>();
    /// send.send((3, response_send));
    /// let (val, response_send) = recv.recv().await.unwrap();
    /// response_send.send(val == 3);
    /// assert_eq!(response_recv.recv().await.unwrap(), true);
    /// # });
    /// ```
    pub fn send(&self, message: T) {
        let mut message = MaybeUninit::new(message);
        // SAFETY: the queue is for `T` and `message` is a valid owned `T`.
        // Additionally, the sender/receiver is only `Send`/`Sync`  if `T` is
        // `Send`/`Sync`.
        let sent = unsafe { self.0.send(MessagePtr::new(&mut message)) };
        if !sent {
            // SAFETY: `message` was not dropped.
            unsafe { message.assume_init_drop() };
        }
    }

    /// Returns whether the receiving side of the channel is known to be closed
    /// (or failed).
    ///
    /// This is useful to determine if there is any point in sending more data
    /// via this port. Note that even if this returns `false` messages may still
    /// fail to reach the destination, for example if the receiver is closed
    /// after this method is called but before the message is consumed.
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

struct MessagePtr(*mut ());

impl MessagePtr {
    fn new<T>(message: &mut MaybeUninit<T>) -> Self {
        Self(message.as_mut_ptr().cast())
    }

    /// # Safety
    /// The caller must ensure that `self` is a valid owned `T`.
    unsafe fn read<T>(self) -> T {
        // SAFETY: The caller guarantees `self` is a valid owned `T`.
        unsafe { self.0.cast::<T>().read() }
    }
}

#[derive(Debug, Clone)]
struct SenderCore(Arc<Queue>);

impl SenderCore {
    /// Sends `message`, taking ownership of it.
    ///
    /// Returns `true` if the message was sent. If `false`, the caller retains
    /// ownership of the message and must drop it.
    ///
    /// # Safety
    /// The caller must ensure that the message is a valid owned `T` for the `T`
    /// the queue was created with. It also must ensure that the queue is not
    /// sent/shared across threads unless `T` is `Send`/`Sync`.
    #[must_use]
    unsafe fn send(&self, message: MessagePtr) -> bool {
        match self.0.access() {
            QueueAccess::Local(mut local) => {
                if local.receiver_gone {
                    return false;
                }
                // SAFETY: The caller guarantees `message` is a valid owned `T`,
                // and that the queue will not be sent/shared across threads
                // unless `T` is `Send`/`Sync`.
                unsafe { local.messages.push_back(message.0) };
                if let Some(waker) = local.waker.take() {
                    drop(local);
                    waker.wake();
                }
            }
            QueueAccess::Remote(remote) => {
                // SAFETY: The caller guarantees `message` is a valid owned `T`.
                let message = unsafe { (remote.encode)(message) };
                remote.port.send(message);
            }
        }
        true
    }

    fn is_closed(&self) -> bool {
        match self.0.access() {
            QueueAccess::Local(local) => local.receiver_gone,
            QueueAccess::Remote(remote) => remote.port.is_closed().unwrap_or(true),
        }
    }

    fn into_queue(self) -> Arc<Queue> {
        let Self(ref queue) = *ManuallyDrop::new(self);
        // SAFETY: copying from a field that won't be dropped.
        unsafe { <*const _>::read(queue) }
    }

    /// Creates a new queue for sending to `port`.
    ///
    /// # Safety
    /// The caller must ensure that both `vtable` and `encode` are for a queue
    /// with the same type.
    unsafe fn from_port(port: Port, vtable: &'static ElementVtable, encode: EncodeFn) -> Self {
        let queue = Arc::new(Queue {
            local: Mutex::new(LocalQueue {
                remote: true,
                ..LocalQueue::new(vtable)
            }),
            remote: OnceLock::from(RemoteQueueState { port, encode }),
        });
        Self(queue)
    }

    /// Converts this sender into a port.
    ///
    /// # Safety
    /// The caller must ensure that `new_handler` is for a queue with the same
    /// element type as this sender.
    unsafe fn into_port(self, new_handler: NewHandlerFn) -> Port {
        match Arc::try_unwrap(self.into_queue()) {
            Ok(mut queue) => {
                if let Some(remote) = queue.remote.into_inner() {
                    // This is the unique owner of the port.
                    remote.port
                } else {
                    assert!(queue.local.get_mut().receiver_gone);
                    let (send, _recv) = Port::new_pair();
                    send
                }
            }
            Err(queue) => {
                // There is a receiver or at least one other sender.
                let (send, recv) = Port::new_pair();
                match queue.access() {
                    QueueAccess::Local(mut local) => {
                        if !local.receiver_gone {
                            local.new_handler = new_handler;
                            local.ports.push(recv);
                            if let Some(waker) = local.waker.take() {
                                drop(local);
                                waker.wake();
                            }
                        }
                    }
                    QueueAccess::Remote(remote) => {
                        remote
                            .port
                            .send(Message::new(ChannelPayload::<()>::Port(recv)));
                    }
                }
                send
            }
        }
    }
}

impl Drop for SenderCore {
    fn drop(&mut self) {
        if self.0.remote.get().is_some() {
            return;
        }
        let mut local = self.0.local.lock();
        // TODO: keep a sender count to avoid needing to wake.
        let waker = local.waker.take();
        drop(local);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T: MeshField> DefaultEncoding for Sender<T> {
    type Encoding = PortField;
}

impl<T: MeshField> From<Port> for Sender<T> {
    fn from(port: Port) -> Self {
        Self::from_port(port)
    }
}

impl<T: MeshField> From<Sender<T>> for Port {
    fn from(sender: Sender<T>) -> Self {
        sender.into_port()
    }
}

impl<T: MeshField> Sender<T> {
    /// Bridges this and `recv` together, consuming both `self` and `recv`. This
    /// makes it so that anything sent to `recv` will be directly sent to this
    /// channel's peer receiver, without a separate relay step. This includes
    /// any data that was previously sent but not yet consumed.
    ///
    /// ```rust
    /// # use mesh_channel_core::*;
    /// let (outer_send, inner_recv) = channel::<u32>();
    /// let (inner_send, mut outer_recv) = channel::<u32>();
    ///
    /// outer_send.send(2);
    /// inner_send.send(1);
    /// inner_send.bridge(inner_recv);
    /// assert_eq!(outer_recv.try_recv().unwrap(), 1);
    /// assert_eq!(outer_recv.try_recv().unwrap(), 2);
    /// ```
    pub fn bridge(self, receiver: Receiver<T>) {
        let sender = self.into_port();
        let receiver = receiver.into_port();
        sender.bridge(receiver);
    }

    /// Encodes `ChannelPayload::Message(message)` into a [`Message`].
    ///
    /// # Safety
    /// The caller must ensure that `message` is a valid owned `T`.
    unsafe fn encode_message(message: MessagePtr) -> Message {
        // SAFETY: The caller guarantees `message` is a valid owned `T`.
        unsafe { Message::new(ChannelPayload::Message(message.read::<T>())) }
    }

    fn from_port(port: Port) -> Self {
        Self(
            // SAFETY: the vtable and encode function are for a queue with type
            // `T`.
            unsafe {
                SenderCore::from_port(
                    port,
                    const { &ElementVtable::new::<T>() },
                    Self::encode_message,
                )
            },
            PhantomData,
        )
    }

    fn into_port(self) -> Port {
        // SAFETY: the new handler function is for a queue with type `T`.
        unsafe { self.0.into_port(RemotePortHandler::new::<T>) }
    }
}

/// The receiving half of a channel returned by [`channel`].
//
// Note that the `PhantomData` here is necessary to ensure `Send/Sync` traits
// are only implemented when `T` is `Send`, since the `ReceiverCore` is always
// `Send+Sync`. This behavior is verified in the unit tests.
pub struct Receiver<T>(ReceiverCore, PhantomData<Arc<Mutex<[T]>>>);

impl<T> Debug for Receiver<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl<T> Default for Receiver<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct ReceiverCore {
    queue: ReceiverQueue,
    ports: PortHandlerList,
    terminated: bool,
}

#[derive(Debug)]
struct ReceiverQueue(Arc<Queue>);

impl Drop for ReceiverQueue {
    fn drop(&mut self) {
        let mut local = self.0.local.lock();
        local.receiver_gone = true;
        let _waker = std::mem::take(&mut local.waker);
        local.messages.clear_and_shrink();
        let _ports = std::mem::take(&mut local.ports);
    }
}

impl<T> Receiver<T> {
    /// Creates a new receiver with no senders.
    ///
    /// Receives will fail with [`RecvError::Closed`] until [`Self::sender`] is
    /// called.
    pub fn new() -> Self {
        Self(
            ReceiverCore::new(const { &ElementVtable::new::<T>() }),
            PhantomData,
        )
    }

    /// Consumes and returns the next message, waiting until one is available.
    ///
    /// Returns immediately when the channel is closed or failed.
    ///
    /// ```rust
    /// # use mesh_channel_core::*;
    /// # futures::executor::block_on(async {
    /// let (send, mut recv) = channel();
    /// send.send(5u32);
    /// drop(send);
    /// assert_eq!(recv.recv().await.unwrap(), 5);
    /// assert!(matches!(recv.recv().await.unwrap_err(), RecvError::Closed));
    /// # });
    /// ```
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv(self)
    }

    /// Consumes and returns the next message, if there is one.
    ///
    /// Otherwise, returns whether the channel is empty, closed, or failed.
    ///
    /// ```rust
    /// # use mesh_channel_core::*;
    /// let (send, mut recv) = channel();
    /// send.send(5u32);
    /// drop(send);
    /// assert_eq!(recv.try_recv().unwrap(), 5);
    /// assert!(matches!(recv.try_recv().unwrap_err(), TryRecvError::Closed));
    /// ```
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut v = MaybeUninit::<T>::uninit();
        // SAFETY: `v` is a valid uninitialized `T`.
        let r = unsafe { self.0.try_poll_recv(None, MessagePtr::new(&mut v)) };
        match r {
            Poll::Ready(Ok(())) => {
                // SAFETY: `try_poll_recv` guarantees `v` is now initialized.
                Ok(unsafe { v.assume_init() })
            }
            Poll::Ready(Err(RecvError::Closed)) => Err(TryRecvError::Closed),
            Poll::Ready(Err(RecvError::Error(e))) => Err(TryRecvError::Error(e)),
            Poll::Pending => Err(TryRecvError::Empty),
        }
    }

    /// Polls for the next message.
    ///
    /// If one is available, consumes and returns it. If the
    /// channel is closed or failed, fails. Otherwise, registers the current task to wake
    /// when a message is available or the channel is closed or fails.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        let mut v = MaybeUninit::<T>::uninit();
        // SAFETY: `v` is a valid uninitialized `T`.
        let r = unsafe { self.0.try_poll_recv(Some(cx), MessagePtr::new(&mut v)) };
        r.map(|r| {
            r.map(|()| {
                // SAFETY: `try_poll_recv` guarantees `v` is now initialized.
                unsafe { v.assume_init() }
            })
        })
    }

    /// Creates a new sender for sending data to this receiver.
    ///
    /// Note that this may transition the channel from the closed to open state.
    pub fn sender(&mut self) -> Sender<T> {
        Sender(self.0.sender(), PhantomData)
    }
}

/// The future returned by [`Receiver::recv`].
pub struct Recv<'a, T>(&'a mut Receiver<T>);

impl<T> Future for Recv<'_, T> {
    type Output = Result<T, RecvError>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().0.poll_recv(cx)
    }
}

impl ReceiverCore {
    fn new(vtable: &'static ElementVtable) -> Self {
        Self {
            queue: ReceiverQueue(Arc::new(Queue {
                local: Mutex::new(LocalQueue::new(vtable)),
                remote: OnceLock::new(),
            })),
            ports: PortHandlerList::new(),
            terminated: true,
        }
    }

    // Polls for a message.
    //
    // # Safety
    // `dst` must be be valid for writing a `T`, the element type of the queue.
    unsafe fn try_poll_recv(
        &mut self,
        cx: Option<&mut Context<'_>>,
        dst: MessagePtr,
    ) -> Poll<Result<(), RecvError>> {
        loop {
            debug_assert!(self.queue.0.remote.get().is_none());
            let mut local = self.queue.0.local.lock();
            if local.remove_closed {
                local.remove_closed = false;
                drop(local);
                if let Err(err) = self.ports.remove_closed() {
                    // Propagate the error to the caller only if there
                    // are no more senders. Otherwise, the caller might
                    // stop receiving messages from the remaining
                    // senders.
                    let local = self.queue.0.local.lock();
                    if local.messages.is_empty() && local.ports.is_empty() && self.is_closed() {
                        self.terminated = true;
                        return Poll::Ready(Err(RecvError::Error(err)));
                    } else {
                        trace_channel_error(&err);
                    }
                }
            } else if !local.ports.is_empty() {
                let new_handler = local.new_handler;
                let ports = std::mem::take(&mut local.ports);
                drop(local);
                self.ports.0.extend(ports.into_iter().map(|port| {
                    // SAFETY: `new_handler` has been set to a function whose
                    // element type matches the queue's element type.
                    let handler = unsafe { new_handler(self.queue.0.clone()) };
                    port.set_handler(handler)
                }));
                continue;
            } else if local.messages.is_empty() {
                if let Some(cx) = cx {
                    if !local
                        .waker
                        .as_ref()
                        .map_or(false, |waker| waker.will_wake(cx.waker()))
                        && !self.is_closed()
                    {
                        local.waker = Some(cx.waker().clone());
                    }
                }
                if self.is_closed() {
                    self.terminated = true;
                    return Poll::Ready(Err(RecvError::Closed));
                } else {
                    return Poll::Pending;
                }
            } else {
                // SAFETY: the caller guarantees `dst` is valid for writing a
                // `T`.
                unsafe { local.messages.pop_front(dst.0) };
                return Poll::Ready(Ok(()));
            }
        }
    }

    fn is_closed(&self) -> bool {
        Arc::strong_count(&self.queue.0) == 1
    }

    fn sender(&mut self) -> SenderCore {
        self.terminated = false;
        SenderCore(self.queue.0.clone())
    }

    /// Converts this receiver into a port.
    ///
    /// # Safety
    /// The caller must ensure that `encode` is for `T`, the element type of
    /// this receiver.
    unsafe fn into_port(mut self, encode: EncodeFn) -> Port {
        let ports = self.ports.into_ports();
        if ports.len() == 1 {
            if let Some(queue) = Arc::get_mut(&mut self.queue.0) {
                let local = queue.local.get_mut();
                if local.messages.is_empty() && local.ports.is_empty() {
                    return ports.into_iter().next().unwrap();
                }
            }
        }
        let (send, recv) = Port::new_pair();
        for port in ports {
            send.send(Message::new(ChannelPayload::<()>::Port(port)));
        }
        let mut local = self.queue.0.local.lock();
        for port in local.ports.drain(..) {
            send.send(Message::new(ChannelPayload::<()>::Port(port)));
        }
        while let Some(message) = local.messages.pop_front_in_place() {
            // SAFETY: `message` is a valid owned `T`.
            let message = unsafe { encode(MessagePtr(message.as_ptr())) };
            send.send(message);
        }
        local.remote = true;
        self.queue
            .0
            .remote
            .set(RemoteQueueState { port: send, encode })
            .ok()
            .unwrap();

        recv
    }

    /// Creates a new queue for receiving from `port`.
    ///
    /// # Safety
    /// The caller must ensure that `vtable` and `new_handler` are for a
    /// consistent queue element type.
    unsafe fn from_port(
        port: Port,
        vtable: &'static ElementVtable,
        new_handler: NewHandlerFn,
    ) -> Self {
        let queue = Arc::new(Queue {
            local: Mutex::new(LocalQueue {
                ports: vec![port],
                new_handler,
                ..LocalQueue::new(vtable)
            }),
            remote: OnceLock::new(),
        });
        Self {
            queue: ReceiverQueue(queue),
            ports: PortHandlerList::new(),
            terminated: false,
        }
    }
}

fn trace_channel_error(err: &ChannelError) {
    tracing::error!(
        error = err as &dyn std::error::Error,
        "channel closed due to error"
    );
}

impl<T> futures_core::Stream for Receiver<T> {
    type Item = T;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(match std::task::ready!(self.get_mut().poll_recv(cx)) {
            Ok(t) => Some(t),
            Err(RecvError::Closed) => None,
            Err(RecvError::Error(err)) => {
                trace_channel_error(&err);
                None
            }
        })
    }
}

impl<T> futures_core::FusedStream for Receiver<T> {
    fn is_terminated(&self) -> bool {
        self.0.terminated
    }
}

#[derive(Debug)]
struct PortHandlerList(Vec<PortWithHandler<RemotePortHandler>>);

impl PortHandlerList {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn remove_closed(&mut self) -> Result<(), ChannelError> {
        let mut r = Ok(());
        self.0.retain(|port| match port.is_closed() {
            Ok(true) => false,
            Ok(false) => true,
            Err(err) => {
                let err = ChannelError::from(err);
                if r.is_ok() {
                    r = Err(err);
                } else {
                    trace_channel_error(&err);
                }
                false
            }
        });
        r
    }

    fn into_ports(self) -> Vec<Port> {
        self.0
            .into_iter()
            .map(|port| port.remove_handler().0)
            .collect()
    }
}

impl<T: MeshField> DefaultEncoding for Receiver<T> {
    type Encoding = PortField;
}

impl<T: MeshField> From<Port> for Receiver<T> {
    fn from(port: Port) -> Self {
        Self::from_port(port)
    }
}

impl<T: MeshField> From<Receiver<T>> for Port {
    fn from(receiver: Receiver<T>) -> Self {
        receiver.into_port()
    }
}

impl<T: MeshField> Receiver<T> {
    /// Bridges this and `sender` together, consuming both `self` and `sender`.
    ///
    /// See [`Sender::bridge`] for more details.
    pub fn bridge(self, sender: Sender<T>) {
        sender.bridge(self)
    }

    fn into_port(self) -> Port {
        // SAFETY: the encode function is for `T`.
        unsafe { self.0.into_port(Sender::<T>::encode_message) }
    }

    fn from_port(port: Port) -> Self {
        Self(
            // SAFETY: the vtable and new handler function are for a queue with
            // type `T`.
            unsafe {
                ReceiverCore::from_port(
                    port,
                    const { &ElementVtable::new::<T>() },
                    RemotePortHandler::new::<T>,
                )
            },
            PhantomData,
        )
    }
}

#[derive(Debug)]
struct Queue {
    remote: OnceLock<RemoteQueueState>,
    local: Mutex<LocalQueue>,
}

enum QueueAccess<'a> {
    Local(MutexGuard<'a, LocalQueue>),
    Remote(&'a RemoteQueueState),
}

impl Queue {
    fn access(&self) -> QueueAccess<'_> {
        loop {
            // Check if the queue is remote first to avoid taking the lock.
            if let Some(remote) = self.remote.get() {
                break QueueAccess::Remote(remote);
            } else {
                let local = self.local.lock();
                if local.remote {
                    // The queue was made remote between our check above and
                    // taking the lock.
                    continue;
                }
                break QueueAccess::Local(local);
            }
        }
    }
}

#[derive(Debug)]
struct LocalQueue {
    messages: ErasedVecDeque,
    ports: Vec<Port>,
    waker: Option<Waker>,
    remote: bool,
    receiver_gone: bool,
    remove_closed: bool,
    new_handler: NewHandlerFn,
}

type NewHandlerFn = unsafe fn(Arc<Queue>) -> RemotePortHandler;

impl LocalQueue {
    fn new(vtable: &'static ElementVtable) -> Self {
        Self {
            messages: ErasedVecDeque::new(vtable),
            ports: Vec::new(),
            waker: None,
            remote: false,
            receiver_gone: false,
            remove_closed: false,
            new_handler: missing_handler,
        }
    }
}

fn missing_handler(_: Arc<Queue>) -> RemotePortHandler {
    unreachable!("handler function not set")
}

#[derive(Debug)]
struct RemoteQueueState {
    port: Port,
    encode: EncodeFn,
}

type EncodeFn = unsafe fn(MessagePtr) -> Message;

#[derive(Protobuf)]
#[mesh(bound = "T: MeshField", resource = "mesh_node::resource::Resource")]
enum ChannelPayload<T> {
    Message(T),
    Port(Port),
}

#[derive(Debug)]
struct RemotePortHandler {
    queue: Arc<Queue>,
    parse: unsafe fn(Message, *mut ()) -> Result<Option<Port>, ChannelError>,
}

impl RemotePortHandler {
    /// Creates a new handler for a queue with element type `T`.
    ///
    /// # Safety
    /// The caller must ensure that `queue` has element type `T`.
    unsafe fn new<T: MeshField>(queue: Arc<Queue>) -> Self {
        Self {
            queue,
            parse: Self::parse::<T>,
        }
    }

    /// Parses a message into a `T` or a `Port`.
    ///
    /// # Safety
    /// The caller must ensure that `p` is valid for writing a `T`.
    unsafe fn parse<T: MeshField>(
        message: Message,
        p: *mut (),
    ) -> Result<Option<Port>, ChannelError> {
        match message.parse::<ChannelPayload<T>>() {
            Ok(ChannelPayload::Message(message)) => {
                // SAFETY: The caller guarantees `p` is valid for writing a `T`.
                unsafe { p.cast::<T>().write(message) };
                Ok(None)
            }
            Ok(ChannelPayload::Port(port)) => Ok(Some(port)),
            Err(err) => Err(err.into()),
        }
    }
}

impl HandlePortEvent for RemotePortHandler {
    fn message(
        &mut self,
        control: &mut mesh_node::local_node::PortControl<'_>,
        message: Message,
    ) -> Result<(), HandleMessageError> {
        let mut local = self.queue.local.lock();
        assert!(!local.receiver_gone);
        assert!(!local.remote);
        // Decode directly into the queue.
        let p = local.messages.reserve_one();
        // SAFETY: `p` is valid for writing a `T`, the element type of the
        // queue.
        let r = unsafe { (self.parse)(message, p.as_ptr()) };
        let port = r.map_err(HandleMessageError::new)?;
        match port {
            None => {
                // SAFETY: `p` has been written to.
                unsafe { p.commit() };
            }
            Some(port) => {
                local.ports.push(port);
            }
        }
        let waker = local.waker.take();
        drop(local);
        if let Some(waker) = waker {
            control.wake(waker);
        }
        Ok(())
    }

    fn close(&mut self, control: &mut mesh_node::local_node::PortControl<'_>) {
        let waker = {
            let mut local = self.queue.local.lock();
            local.remove_closed = true;
            local.waker.take()
        };
        if let Some(waker) = waker {
            control.wake(waker);
        }
    }

    fn fail(
        &mut self,
        control: &mut mesh_node::local_node::PortControl<'_>,
        _err: mesh_node::local_node::NodeError,
    ) {
        self.close(control);
    }

    fn drain(&mut self) -> Vec<Message> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::channel;
    use super::Receiver;
    use super::Sender;
    use crate::RecvError;
    use futures::executor::block_on;
    use futures::StreamExt;
    use futures_core::FusedStream;
    use mesh_node::local_node::Port;
    use std::cell::Cell;
    use test_with_tracing::test;

    // Ensure `Send` and `Sync` are implemented correctly.
    static_assertions::assert_impl_all!(Sender<i32>: Send, Sync);
    static_assertions::assert_impl_all!(Receiver<i32>: Send, Sync);
    static_assertions::assert_impl_all!(Sender<Cell<i32>>: Send, Sync);
    static_assertions::assert_impl_all!(Receiver<Cell<i32>>: Send, Sync);
    static_assertions::assert_not_impl_any!(Sender<*const ()>: Send, Sync);
    static_assertions::assert_not_impl_any!(Receiver<*const ()>: Send, Sync);

    #[test]
    fn test_basic() {
        block_on(async {
            let (sender, mut receiver) = channel();
            sender.send(String::from("test"));
            assert_eq!(receiver.next().await.as_deref(), Some("test"));
            drop(sender);
            assert_eq!(receiver.next().await, None);
        })
    }

    #[test]
    fn test_convert_sender_port() {
        block_on(async {
            let (sender, mut receiver) = channel::<String>();
            let sender = Sender::<String>::from(Port::from(sender));
            sender.send(String::from("test"));
            assert_eq!(receiver.next().await.as_deref(), Some("test"));
            drop(sender);
            assert_eq!(receiver.next().await, None);
        })
    }

    #[test]
    fn test_convert_receiver_port() {
        block_on(async {
            let (sender, receiver) = channel();
            let mut receiver = Receiver::<String>::from(Port::from(receiver));
            sender.send(String::from("test"));
            assert_eq!(receiver.next().await.as_deref(), Some("test"));
            drop(sender);
            assert_eq!(receiver.next().await, None);
        })
    }

    #[test]
    fn test_non_port_and_port_sender() {
        block_on(async {
            let (sender, mut receiver) = channel();
            let sender2 = Sender::<String>::from(Port::from(sender.clone()));
            sender.send(String::from("test"));
            sender2.send(String::from("tset"));
            assert_eq!(receiver.next().await.as_deref(), Some("test"));
            assert_eq!(receiver.next().await.as_deref(), Some("tset"));
            drop(sender);
            drop(sender2);
            assert_eq!(receiver.next().await, None);
        })
    }

    #[test]
    fn test_port_receiver_with_senders_and_messages() {
        block_on(async {
            let (sender, receiver) = channel();
            let sender2 = Sender::<String>::from(Port::from(sender.clone()));
            sender.send(String::from("test"));
            sender2.send(String::from("tset"));
            let mut receiver = Receiver::<String>::from(Port::from(receiver));
            assert_eq!(receiver.next().await.as_deref(), Some("test"));
            assert_eq!(receiver.next().await.as_deref(), Some("tset"));
            drop(sender);
            drop(sender2);
            assert_eq!(receiver.next().await, None);
        })
    }

    #[test]
    fn test_message_corruption() {
        block_on(async {
            let (sender, receiver) = channel();
            let mut receiver = Receiver::<i32>::from(Port::from(receiver));
            sender.send("text".to_owned());
            let RecvError::Error(err) = receiver.recv().await.unwrap_err() else {
                panic!()
            };
            tracing::info!(error = &err as &dyn std::error::Error, "expected error");
            assert!(receiver.is_terminated());
        })
    }
}
