// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of an async multi-producer, single-consumer (MPSC) channel
//! that can be used to communicate between mesh nodes.
//!
//! The main design requirements of this channel are:
//! * It roughly follows the semantics of the Rust standard library's
//!   `std::sync::mpsc` channel, but with async support.
//! * It is efficient enough for single process use that it can be used as a
//!   general purpose channel.
//! * It leverages `mesh_node` ports and `mesh_protobuf` serialization to allow
//!   communication between mesh nodes, which can be on different processes or
//!   machines.
//! * Its contribution to binary size is minimal.
//!
//! To achieve the binary size goal, this implementation avoids generics where
//! practical. This has the tradeoff of requiring a fair amount of unsafe code,
//! but this makes it practical to use this channel in space-constrained
//! environments.

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
use mesh_node::message::OwnedMessage;
use mesh_protobuf::DefaultEncoding;
use mesh_protobuf::Protobuf;
use parking_lot::Mutex;
use parking_lot::MutexGuard;
use std::marker::PhantomPinned;
use std::sync::Arc;
use std::sync::OnceLock;
use std::task::ready;

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
        // SAFETY: the queue is for `T` and `message` is a valid owned `T`.
        // Additionally, the sender/receiver is only `Send`/`Sync` if `T` is
        // `Send`/`Sync`.
        unsafe { self.0.send(message) }
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

/// Sends a `ChannelPayload::Message(message)` to a port.
///
/// # Safety
/// The caller must ensure that `message` is a valid owned `T`.
unsafe fn send_message<T: MeshField>(port: &Port, message: MessagePtr) {
    // SAFETY: The caller guarantees `message` is a valid owned `T`.
    let m = unsafe { ChannelPayload::Message(message.read::<T>()) };
    port.send_protobuf(m);
}

#[derive(Debug, Clone)]
struct SenderCore(ManuallyDrop<Arc<Queue>>);

impl SenderCore {
    /// Sends `message`.
    ///
    /// # Safety
    /// The caller must ensure that the message is a valid owned `T` for the `T`
    /// the queue was created with. It also must ensure that the queue is not
    /// sent/shared across threads unless `T` is `Send`/`Sync`.
    unsafe fn send<T>(&self, message: T) {
        fn send(queue: &Queue, message: MessagePtr) -> bool {
            match queue.access() {
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
                    unsafe { (remote.send)(&remote.port, message) };
                }
            }
            true
        }

        let mut message = MaybeUninit::new(message);
        let sent = send(&self.0, MessagePtr::new(&mut message));
        if !sent {
            // SAFETY: `message` was not dropped.
            unsafe { message.assume_init_drop() };
        }
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
        unsafe { <*const _>::read(&**queue) }
    }

    /// Creates a new queue with element type `T` for sending to `port`.
    fn from_port<T: MeshField>(port: Port) -> Self {
        fn from_port(port: Port, vtable: &'static ElementVtable, send: SendFn) -> SenderCore {
            SenderCore(ManuallyDrop::new(Arc::new(Queue {
                local: Mutex::new(LocalQueue {
                    remote: true,
                    ..LocalQueue::new(vtable)
                }),
                remote: OnceLock::from(RemoteQueueState { port, send }),
            })))
        }

        from_port(
            port,
            const { &ElementVtable::new::<T>() },
            send_message::<T>,
        )
    }

    /// Converts this sender into a port.
    ///
    /// # Safety
    /// The caller must ensure that the queue has element type `T`.
    unsafe fn into_port<T: MeshField>(self) -> Port {
        fn into_port(this: SenderCore, new_handler: NewHandlerFn) -> Port {
            match Arc::try_unwrap(this.into_queue()) {
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
                            remote.port.send_protobuf(ChannelPayload::<()>::Port(recv));
                        }
                    }
                    send
                }
            }
        }
        into_port(self, RemotePortHandler::new::<T>)
    }
}

impl Drop for SenderCore {
    fn drop(&mut self) {
        // SAFETY: the queue won't be referenced after this.
        let queue = unsafe { ManuallyDrop::take(&mut self.0) };
        let waker = if queue.remote.get().is_some() {
            None
        } else {
            let mut local = queue.local.lock();
            // TODO: keep a sender count to avoid needing to wake.
            local.waker.take()
        };
        // Drop the queue so that the receiver will see the sender is gone.
        drop(queue);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> DefaultEncoding for Sender<T> {
    type Encoding = PortField;
}

impl<T: MeshField> From<Port> for Sender<T> {
    fn from(port: Port) -> Self {
        Self(SenderCore::from_port::<T>(port), PhantomData)
    }
}

impl<T: MeshField> From<Sender<T>> for Port {
    fn from(sender: Sender<T>) -> Self {
        // SAFETY: the queue has element type `T`.
        unsafe { sender.0.into_port::<T>() }
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
        let sender = Port::from(self);
        let receiver = Port::from(receiver);
        sender.bridge(receiver);
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
        Recv(self, PhantomPinned)
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
        // SAFETY: the queue type is `T`.
        let r = unsafe { self.0.try_poll_recv::<T>(None) };
        match r {
            Poll::Ready(Ok(v)) => Ok(v),
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
        // SAFETY: the queue type is `T`.
        unsafe { self.0.try_poll_recv(Some(cx)) }
    }

    /// Creates a new sender for sending data to this receiver.
    ///
    /// Note that this may transition the channel from the closed to open state.
    pub fn sender(&mut self) -> Sender<T> {
        Sender(self.0.sender(), PhantomData)
    }
}

/// The future returned by [`Receiver::recv`].
//
// Force `!Unpin` to allow for future optimizations.
pub struct Recv<'a, T>(&'a mut Receiver<T>, PhantomPinned);

impl<T> Future for Recv<'_, T> {
    type Output = Result<T, RecvError>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: there are no actual pinning invariants.
        let this = unsafe { self.get_unchecked_mut() };
        this.0.poll_recv(cx)
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
    // The queue must have element type `T`.
    unsafe fn try_poll_recv<T>(
        &mut self,
        cx: Option<&mut Context<'_>>,
    ) -> Poll<Result<T, RecvError>> {
        fn try_poll_recv<'a>(
            this: &'a mut ReceiverCore,
            cx: Option<&mut Context<'_>>,
        ) -> Poll<Result<MutexGuard<'a, LocalQueue>, RecvError>> {
            loop {
                debug_assert!(this.queue.0.remote.get().is_none());
                let mut local = this.queue.0.local.lock();
                if local.remove_closed {
                    local.remove_closed = false;
                    drop(local);
                    if let Err(err) = this.ports.remove_closed() {
                        // Propagate the error to the caller only if there
                        // are no more senders. Otherwise, the caller might
                        // stop receiving messages from the remaining
                        // senders.
                        let local = this.queue.0.local.lock();
                        if local.messages.is_empty() && local.ports.is_empty() && this.is_closed() {
                            this.terminated = true;
                            return Poll::Ready(Err(RecvError::Error(err)));
                        } else {
                            trace_channel_error(&err);
                        }
                    }
                } else if !local.ports.is_empty() {
                    let new_handler = local.new_handler;
                    let ports = std::mem::take(&mut local.ports);
                    drop(local);
                    this.ports.0.extend(ports.into_iter().map(|port| {
                        // SAFETY: `new_handler` has been set to a function whose
                        // element type matches the queue's element type.
                        let handler = unsafe { new_handler(this.queue.0.clone()) };
                        port.set_handler(handler)
                    }));
                    continue;
                } else if local.messages.is_empty() {
                    if let Some(cx) = cx {
                        if !local
                            .waker
                            .as_ref()
                            .map_or(false, |waker| waker.will_wake(cx.waker()))
                            && !this.is_closed()
                        {
                            local.waker = Some(cx.waker().clone());
                        }
                    }
                    if this.is_closed() {
                        this.terminated = true;
                        return Poll::Ready(Err(RecvError::Closed));
                    } else {
                        return Poll::Pending;
                    }
                } else {
                    return Poll::Ready(Ok(local));
                }
            }
        }

        ready!(try_poll_recv(self, cx))
            .map(|mut local| {
                let message = local.messages.pop_front_in_place().unwrap();
                // SAFETY: `message` is a valid owned `T`.
                unsafe { message.as_ptr().cast::<T>().read() }
            })
            .into()
    }

    fn is_closed(&self) -> bool {
        Arc::strong_count(&self.queue.0) == 1
    }

    fn sender(&mut self) -> SenderCore {
        self.terminated = false;
        SenderCore(ManuallyDrop::new(self.queue.0.clone()))
    }

    /// Converts this receiver into a port.
    ///
    /// # Safety
    /// The caller must ensure that the queue has element type `T`.
    unsafe fn into_port<T: MeshField>(self) -> Port {
        fn into_port(mut this: ReceiverCore, send: SendFn) -> Port {
            let ports = this.ports.into_ports();
            if ports.len() == 1 {
                if let Some(queue) = Arc::get_mut(&mut this.queue.0) {
                    let local = queue.local.get_mut();
                    if local.messages.is_empty() && local.ports.is_empty() {
                        return ports.into_iter().next().unwrap();
                    }
                }
            }
            let (sender, recv) = Port::new_pair();
            for port in ports {
                sender.send_protobuf(ChannelPayload::<()>::Port(port));
            }
            let mut local = this.queue.0.local.lock();
            for port in local.ports.drain(..) {
                sender.send_protobuf(ChannelPayload::<()>::Port(port));
            }
            while let Some(message) = local.messages.pop_front_in_place() {
                // SAFETY: `message` is a valid owned `T`.
                unsafe { send(&sender, MessagePtr(message.as_ptr())) };
            }
            local.remote = true;
            this.queue
                .0
                .remote
                .set(RemoteQueueState { port: sender, send })
                .ok()
                .unwrap();

            recv
        }
        into_port(self, send_message::<T>)
    }

    /// Creates a new queue with element type `T` for receiving from `port`.
    fn from_port<T: MeshField>(port: Port) -> Self {
        fn from_port(
            port: Port,
            vtable: &'static ElementVtable,
            new_handler: NewHandlerFn,
        ) -> ReceiverCore {
            let queue = Arc::new(Queue {
                local: Mutex::new(LocalQueue {
                    ports: vec![port],
                    new_handler,
                    ..LocalQueue::new(vtable)
                }),
                remote: OnceLock::new(),
            });
            ReceiverCore {
                queue: ReceiverQueue(queue),
                ports: PortHandlerList::new(),
                terminated: false,
            }
        }
        from_port(
            port,
            const { &ElementVtable::new::<T>() },
            RemotePortHandler::new::<T>,
        )
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
        Self(ReceiverCore::from_port::<T>(port), PhantomData)
    }
}

impl<T: MeshField> From<Receiver<T>> for Port {
    fn from(receiver: Receiver<T>) -> Self {
        // SAFETY: the queue has element type `T`.
        unsafe { receiver.0.into_port::<T>() }
    }
}

impl<T: MeshField> Receiver<T> {
    /// Bridges this and `sender` together, consuming both `self` and `sender`.
    ///
    /// See [`Sender::bridge`] for more details.
    pub fn bridge(self, sender: Sender<T>) {
        sender.bridge(self)
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
    send: SendFn,
}

type SendFn = unsafe fn(&Port, MessagePtr);

#[derive(Protobuf)]
#[mesh(bound = "T: MeshField", resource = "mesh_node::resource::Resource")]
enum ChannelPayload<T> {
    #[mesh(transparent)]
    Message(T),
    #[mesh(transparent)]
    Port(Port),
}

struct RemotePortHandler {
    queue: Arc<Queue>,
    parse: unsafe fn(Message<'_>, *mut ()) -> Result<Option<Port>, ChannelError>,
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
        message: Message<'_>,
        p: *mut (),
    ) -> Result<Option<Port>, ChannelError> {
        match message.parse_non_static::<ChannelPayload<T>>() {
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
        control: &mut mesh_node::local_node::PortControl<'_, '_>,
        message: Message<'_>,
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

    fn close(&mut self, control: &mut mesh_node::local_node::PortControl<'_, '_>) {
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
        control: &mut mesh_node::local_node::PortControl<'_, '_>,
        _err: mesh_node::local_node::NodeError,
    ) {
        self.close(control);
    }

    fn drain(&mut self) -> Vec<OwnedMessage> {
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
    use mesh_protobuf::Protobuf;
    use std::cell::Cell;
    use std::marker::PhantomData;
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

    #[test]
    fn test_no_send() {
        block_on(async {
            #[derive(Protobuf)]
            struct NoSend(String, PhantomData<*mut ()>);

            let (sender, receiver) = channel::<NoSend>();
            let mut receiver = Receiver::<NoSend>::from(Port::from(receiver));
            sender.send(NoSend(String::from("test"), PhantomData));
            assert_eq!(
                receiver.next().await.as_ref().map(|v| v.0.as_str()),
                Some("test")
            );
        })
    }
}
