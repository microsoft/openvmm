// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod protocol;

use crate::common::Address;
use crate::common::NodeId;
use crate::common::PortId;
use crate::message::Message;
use crate::message::OwnedMessage;
use crate::resource::OsResource;
use crate::resource::Resource;
use mesh_protobuf::DefaultEncoding;
use mesh_protobuf::buffer::Buf;
use mesh_protobuf::buffer::Buffer;
use mesh_protobuf::buffer::write_with;
use mesh_protobuf::protobuf::Encoder;
use parking_lot::Mutex;
use parking_lot::MutexGuard;
use parking_lot::RwLock;
use std::any::Any;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::collections::hash_map;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::marker::PhantomData;
use std::num::Wrapping;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::Ordering;
use std::task::Waker;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;
use zerocopy::Ref;
use zerocopy::Unalign;

/// One half of a bidirectional communication channel.
///
/// This is a lower-level construct for sending and receiving binary messages.
/// Most code should use a higher-level channel returned by
/// `mesh_channel::channel()`, which uses this type internally.
pub struct Port {
    inner: Arc<PortInner>,
}

impl Debug for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Debug::fmt(&self.inner.id, f)
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        self.inner.close(None);
    }
}

impl Port {
    /// Creates a new bidirectional channel, returning a pair of ports.
    pub fn new_pair() -> (Self, Self) {
        let left_addr = Address {
            node: NodeId::ZERO,
            port: PortId::new(),
        };
        let right_addr = Address {
            node: NodeId::ZERO,
            port: PortId::new(),
        };
        let left = Self::new(
            left_addr.port,
            PortInnerState::new(PortActivity::Unreachable),
        );
        let right = Self::new(
            right_addr.port,
            PortInnerState::new(PortActivity::Peered(PortRef::LocalPort(left.inner.clone()))),
        );
        left.inner.state.lock().activity =
            PortActivity::Peered(PortRef::LocalPort(right.inner.clone()));
        tracing::trace!(left = ?left_addr.port, right = ?right_addr.port, "new port pair");
        (left, right)
    }

    /// Creates a new port with `id` and initial state `state`.
    fn new(id: PortId, state: PortInnerState) -> Self {
        Self {
            inner: Arc::new(PortInner {
                id,
                state: Mutex::new(state),
            }),
        }
    }

    /// Sets the handler for incoming messages.
    ///
    /// If there are any queued incoming messages, or if the port has already
    /// been closed or failed, then the relevant handler methods will be called
    /// directly on this thread.
    pub fn set_handler<T: HandlePortEvent>(self, handler: T) -> PortWithHandler<T> {
        self.inner.set_handler(Box::new(handler));
        PortWithHandler {
            raw: self,
            _phantom: PhantomData,
        }
    }

    /// Drop this object without closing the underlying port.
    fn forget(self) {
        self.into_inner();
    }

    /// If the port is done (the peer port is closed), then creates a new local
    /// peer port, returning the (open) peer. This is needed when sending a done
    /// port, since only peered ports can be sent.
    fn repeer_if_done(&self, state: &mut PortInnerState) -> Option<Self> {
        if matches!(state.activity, PortActivity::Done) {
            let new_id = PortId::new();
            let mut peer_state =
                PortInnerState::new(PortActivity::Peered(PortRef::LocalPort(self.inner.clone())));
            // Continue this peer from the last sequence number.
            peer_state.next_local_seq = state.event_queue.next_peer_seq;
            let peer_port = Self::new(new_id, peer_state);
            state.set_activity(PortActivity::Peered(PortRef::LocalPort(
                peer_port.inner.clone(),
            )));
            Some(peer_port)
        } else {
            None
        }
    }

    /// Prepares to send a port to another node. This consumes the `Port` and
    /// returns the port data to send.
    fn prepare_to_send(self, remote_node: &Arc<RemoteNode>) -> protocol::ResourceData {
        let old_address = Address {
            node: remote_node.local_node.id,
            port: self.inner.id,
        };

        let port_id = PortId::new();
        let target = PortRef::RemotePort(remote_node.clone(), port_id);

        // Ensure the port is associated with this mesh.
        let mut state = PortInner::associate(&self.inner, &remote_node.local_node);

        // Save a local sequence number for the ChangePeer message.
        let next_local_seq = state.next_local_seq + Wrapping(1);

        // Re-peer the port if its peer is gone. The new peer will be closed
        // again after updating the port state below so that the close will be
        // proxied to the new node.
        let mut _port_to_close = self.repeer_if_done(&mut state);

        // Prepare the port for proxying and get the peer address. Get the peer
        // port's address, associating the peer with the mesh if it is local.
        let mut port_to_associate = None;
        let (peer_node, peer_port) =
            match std::mem::replace(&mut state.activity, PortActivity::Unreachable) {
                PortActivity::Peered(peer) => {
                    let peer_addr = match &peer {
                        PortRef::LocalPort(peer_port) => {
                            port_to_associate = Some(peer_port.clone());
                            (remote_node.local_node.id, Some(peer_port.id))
                        }
                        PortRef::RemotePort(peer_node, peer_port_id) => {
                            (peer_node.id, Some(*peer_port_id))
                        }
                    };
                    state.set_activity(PortActivity::Sending { peer, target });
                    peer_addr
                }
                PortActivity::Failed(err) => {
                    let node_id = *err.node_id().unwrap_or(&remote_node.local_node.id);
                    state.activity = PortActivity::Failed(err);
                    (node_id, None)
                }
                state => panic!("invalid state: {:?}", state),
            };

        drop(state);
        if let Some(port_to_associate) = &port_to_associate {
            drop(PortInner::associate(
                port_to_associate,
                &remote_node.local_node,
            ))
        }

        self.forget();

        protocol::ResourceData {
            id: port_id.0.into(),
            next_local_seq: next_local_seq.0,
            reserved: 0,
            old_node: old_address.node.0.into(),
            old_port: old_address.port.0.into(),
            peer_node: peer_node.0.into(),
            peer_port: peer_port.map_or(protocol::Uuid::ZERO, |p| p.0.into()),
        }
    }

    /// Bridges two channels together so that the peer of `self` is connected
    /// directly to the peer of `other`.
    pub fn bridge(self, other: Self) {
        tracing::trace!(left = ?self.inner.id, right = ?other.inner.id, "bridging ports");

        let get_peer_info = |state: &PortInnerState| {
            match &state.activity {
                PortActivity::Peered(peer) => {
                    let peer = peer.clone();
                    // Save a local sequence number for the ChangePeer message.
                    let initial_seq = state.next_local_seq + Wrapping(1);
                    Ok((peer, initial_seq))
                }
                PortActivity::Failed(err) => Err(err.clone()),
                s => unreachable!("{:?}", s),
            }
        };

        let start_proxy = |inner: &PortInner,
                           state: &mut PortInnerState,
                           target_info: Result<(PortRef, Seq), NodeError>,
                           pending_events: &mut PendingEvents<'_>| {
            let result = match target_info {
                Ok((PortRef::LocalPort(ref target), _)) if target.id == inner.id => {
                    // TODO: can this still happen in a loop?
                    Err(NodeError::local(PortError::CircularBridge))
                }
                Ok((target, initial_seq)) => {
                    match std::mem::replace(&mut state.activity, PortActivity::Unreachable) {
                        PortActivity::Peered(peer) => {
                            state.start_proxy(peer, target, initial_seq, pending_events);
                            Ok(())
                        }
                        activity @ PortActivity::Failed(_) => {
                            state.activity = activity;
                            Ok(())
                        }
                        s => unreachable!("{s:?}"),
                    }
                }
                Err(err) => Err(err),
            };
            if let Err(err) = result {
                state.fail(pending_events, err);
                inner.disassociate(&mut *state);
            }
        };

        let (_this_repeer, _other_repeer);
        let mut pending_events = PendingEvents::new();
        {
            let (mut this_state, mut other_state) = PortInner::lock_two(&self.inner, &other.inner);
            // Ensure both ports have peers by creating local synthetic ports,
            // effectively reopening the ports. These new peers will be closed
            // at function end, re-closing the ports after the bridge.
            _this_repeer = self.repeer_if_done(&mut this_state);
            _other_repeer = other.repeer_if_done(&mut other_state);
            let this_peer_info = get_peer_info(&this_state);
            let other_peer_info = get_peer_info(&other_state);
            start_proxy(
                &self.inner,
                &mut this_state,
                other_peer_info,
                &mut pending_events,
            );
            start_proxy(
                &other.inner,
                &mut other_state,
                this_peer_info,
                &mut pending_events,
            );
        }

        pending_events.process();
        self.forget();
        other.forget();
    }

    /// Sends a message to the peer.
    pub fn send(&self, message: Message<'_>) {
        self.inner.send(message);
    }

    /// Sends a message to the peer and closes the port in one operation.
    pub fn send_and_close(self, message: Message<'_>) {
        // Avoid double close: don't run the drop impl.
        self.into_inner().close(Some(message));
    }

    /// Send a protobuf-encodable message to the peer.
    ///
    /// Prefer [`Port::send`] if you already have a [`Message`],
    /// [`OwnedMessage`], or serialized message, or if the recipient is known to
    /// take advantage of the [`OwnedMessage::try_unwrap`] optimization.
    ///
    /// Otherwise, this method is more efficient since it can avoid an extra
    /// allocation to construct a [`Message`].
    pub fn send_protobuf<T: DefaultEncoding>(&self, value: T)
    where
        T::Encoding: mesh_protobuf::MessageEncode<T, Resource>,
    {
        self.send(crate::message::stack_message!(value));
    }

    /// Send a protobuf-encodable message to the peer and close the port in one
    /// operation.
    ///
    /// Prefer [`Port::send_and_close`] if you already have a [`Message`],
    /// [`OwnedMessage`], or serialized message, or if the recipient is known to
    /// take advantage of the [`OwnedMessage::try_unwrap`] optimization.
    ///
    /// Otherwise, this method is more efficient since it can avoid an extra
    /// allocation to construct a [`Message`].
    pub fn send_protobuf_and_close<T: DefaultEncoding>(self, value: T)
    where
        T::Encoding: mesh_protobuf::MessageEncode<T, Resource>,
    {
        self.send_and_close(crate::message::stack_message!(value));
    }

    pub fn is_closed(&self) -> Result<bool, NodeError> {
        match &self.inner.state.lock().activity {
            PortActivity::Done => Ok(true),
            PortActivity::Failed(err) => Err(err.clone()),
            _ => Ok(false),
        }
    }

    #[cfg(test)]
    fn fail(self, err: NodeError) {
        let mut pending_events = PendingEvents::new();
        {
            let mut state = self.inner.state.lock();
            state.fail(&mut pending_events, err);
        }
        pending_events.process();
    }
}

/// A [`Port`] that has a registered message handler.
///
/// Created by [`Port::set_handler`].
pub struct PortWithHandler<T> {
    raw: Port,
    _phantom: PhantomData<Arc<Mutex<T>>>,
}

impl<T> Debug for PortWithHandler<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortWithHandler")
            .field("raw", &self.raw)
            .finish()
    }
}

impl<T: HandlePortEvent> From<PortWithHandler<T>> for Port {
    fn from(port: PortWithHandler<T>) -> Self {
        port.remove_handler().0
    }
}

impl<T: Default + HandlePortEvent> From<Port> for PortWithHandler<T> {
    fn from(port: Port) -> Self {
        port.set_handler(Default::default())
    }
}

/// Scoped unsafe code with a safe interface.
mod unsafe_code {
    // UNSAFETY: needed to destructure objects that have `Drop` implementations.
    #![expect(unsafe_code)]

    use super::Port;
    use super::PortInner;
    use super::PortWithHandler;
    use std::mem::ManuallyDrop;
    use std::sync::Arc;

    impl Port {
        pub(super) fn into_inner(self) -> Arc<PortInner> {
            let Self { ref inner } = *ManuallyDrop::new(self);
            // SAFETY: copying from a field that won't be dropped.
            unsafe { <*const _>::read(inner) }
        }
    }

    impl<T> PortWithHandler<T> {
        pub(super) fn into_port_preserve_handler(self) -> Port {
            let Self {
                ref raw,
                _phantom: _,
            } = *ManuallyDrop::new(self);
            // SAFETY: copying from a field that won't be dropped.
            unsafe { <*const _>::read(raw) }
        }
    }
}

impl<T: HandlePortEvent> PortWithHandler<T> {
    /// Sends a message to the opposite endpoint.
    pub fn send(&self, message: Message<'_>) {
        self.raw.send(message)
    }

    pub fn is_closed(&self) -> Result<bool, NodeError> {
        self.raw.is_closed()
    }

    pub fn remove_handler(self) -> (Port, T) {
        let port = self.into_port_preserve_handler();
        let handler = port.inner.drain_queue().unwrap() as Box<dyn Any>;
        (port, *handler.downcast().unwrap())
    }

    pub fn with_handler<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut state = self.raw.inner.state.lock();
        let handler = state.handler.as_mut().unwrap().as_mut() as &mut dyn Any;
        f(handler.downcast_mut().unwrap())
    }

    pub fn with_port_and_handler<'a, R>(
        &self,
        f: impl FnOnce(&mut PortControl<'_, 'a>, &mut T) -> R,
    ) -> R {
        let mut pending_events = PendingEvents::new();
        let mut state = self.raw.inner.state.lock();
        let state = &mut *state;
        let peer_and_seq = match &state.activity {
            PortActivity::Peered(peer) => Some((peer, &mut state.next_local_seq)),
            _ => None,
        };
        let mut control = PortControl {
            peer_and_seq,
            events: &mut pending_events,
        };
        let handler = state.handler.as_mut().unwrap().as_mut() as &mut dyn Any;
        let r = f(&mut control, handler.downcast_mut().unwrap());
        pending_events.process();
        r
    }
}

/// A field encoder for mesh ports.
pub struct PortField;

impl<T: Into<Port>, R: From<Port>> mesh_protobuf::FieldEncode<T, R> for PortField {
    fn write_field(item: T, writer: mesh_protobuf::protobuf::FieldWriter<'_, '_, R>) {
        writer.resource(item.into().into());
    }

    fn compute_field_size(_item: &mut T, sizer: mesh_protobuf::protobuf::FieldSizer<'_>) {
        sizer.resource();
    }
}

#[derive(Debug, Error)]
#[error("missing port")]
struct MissingPort;

impl<T: From<Port>, R> mesh_protobuf::FieldDecode<'_, T, R> for PortField
where
    Port: TryFrom<R>,
    <Port as TryFrom<R>>::Error: 'static + std::error::Error + Send + Sync,
{
    fn read_field(
        item: &mut mesh_protobuf::inplace::InplaceOption<'_, T>,
        reader: mesh_protobuf::protobuf::FieldReader<'_, '_, R>,
    ) -> mesh_protobuf::Result<()> {
        item.set(
            Port::try_from(reader.resource()?)
                .map_err(mesh_protobuf::Error::new)?
                .into(),
        );
        Ok(())
    }

    fn default_field(
        _item: &mut mesh_protobuf::inplace::InplaceOption<'_, T>,
    ) -> mesh_protobuf::Result<()> {
        Err(mesh_protobuf::Error::new(MissingPort))
    }
}

impl DefaultEncoding for Port {
    type Encoding = PortField;
}

/// The core local node implementation for the transport-specific meshes.
pub struct LocalNode {
    inner: Arc<LocalNodeInner>,
    connector: Mutex<Option<Box<dyn Connect>>>,
}

impl Drop for LocalNode {
    fn drop(&mut self) {
        let err = NodeError::shutting_down();
        // Fail any ports that are still associated so that any
        // remaining circular references are dropped.
        self.inner.fail_all_ports(err.clone());
        // Fail any nodes so that any circular references due to connection
        // objects are dropped.
        self.inner.fail_all_nodes(err);
    }
}

/// The inner state for [`LocalNode`].
#[derive(Debug)]
struct LocalNodeInner {
    id: NodeId,
    state: Mutex<LocalNodeState>,
}

/// A 64-bit message sequence number.
type Seq = Wrapping<u64>;

/// A value with a sequence number, whose order is defined by the sequence
/// number's order.
#[derive(Debug, Copy, Clone)]
struct SeqValue<T>(Seq, T);

impl<T> PartialEq for SeqValue<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T> Eq for SeqValue<T> {}

impl<T> PartialOrd for SeqValue<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for SeqValue<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

/// A connection to a remote node.
struct RemoteNode {
    id: NodeId,
    /// The local node associated with this node.
    ///
    /// Note that this forms a circular reference while the owning `LocalNode`
    /// is alive. When the LocalNode is dropped, the reference from
    /// LocalNodeInner to this object will be released, breaking the cycle.
    local_node: Arc<LocalNodeInner>,
    state: RwLock<RemoteNodeState>,
    failed: AtomicBool,
    node_error: Mutex<Result<(), NodeError>>,
    handle_count: AtomicIsize,
}

impl Debug for RemoteNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteNode")
            .field("local_node", &self.local_node.id)
            .field("id", &self.id)
            .field("failed", &self.failed)
            .finish()
    }
}

/// The interior state of [`RemoteNode`].
enum RemoteNodeState {
    Queuing(Mutex<Vec<DeferredEvent>>),
    Failed,
    Active(Box<dyn SendEvent>),
}

/// A node event that has been pended, waiting for a remote node connection to
/// complete.
#[derive(Debug)]
struct DeferredEvent {
    port_id: PortId,
    seq: Seq,
    event: OwnedPortEvent,
}

impl RemoteNode {
    fn new(local_node: Arc<LocalNodeInner>, id: NodeId) -> (Arc<Self>, RemoteNodeHandle) {
        let this = Arc::new(Self {
            local_node,
            id,
            state: RwLock::new(RemoteNodeState::Queuing(Default::default())),
            failed: AtomicBool::new(false),
            node_error: Mutex::new(Ok(())),
            handle_count: AtomicIsize::new(1),
        });
        let handle = RemoteNodeHandle {
            id,
            remote_node: Arc::downgrade(&this),
        };
        (this, handle)
    }

    /// Provides a connection for the remote node, flushing any deferred events.
    fn connect(self: &Arc<Self>, conn: Box<dyn SendEvent>) -> bool {
        let events = {
            let mut state = self.state.write();
            match &mut *state {
                RemoteNodeState::Queuing(v) => {
                    let v = std::mem::take(v.get_mut());
                    *state = RemoteNodeState::Active(conn);
                    v
                }
                _ => return false,
            }
        };
        self.check_failed();
        for event in events {
            self.event(event.port_id, event.seq, event.event.into());
        }
        true
    }

    fn check_failed(&self) {
        if self.failed.load(Ordering::SeqCst) {
            let _old = std::mem::replace(&mut *self.state.write(), RemoteNodeState::Failed);
        }
    }

    /// Fails a remote node connection.
    fn fail(&self, err: NodeError) {
        *self.node_error.lock() = Err(err);
        self.failed.store(true, Ordering::SeqCst);
        // Try to remove the connection immediately. This may fail if the lock
        // is held elsewhere--those callers will double check the failed bit
        // once they unlock the lock.
        if let Some(mut state) = self.state.try_write() {
            let _old = std::mem::replace(&mut *state, RemoteNodeState::Failed);
        }
    }

    /// Sends an event to the remote node.
    fn event(self: &Arc<Self>, port_id: PortId, seq: Seq, event: PortEvent<'_>) {
        match &*self.state.read() {
            RemoteNodeState::Queuing(v) => {
                v.lock().push(DeferredEvent {
                    port_id,
                    seq,
                    event: event.into_owned(),
                });
            }
            RemoteNodeState::Failed => (),
            RemoteNodeState::Active(conn) => {
                conn.event(OutgoingEvent::new(port_id, seq, event, self))
            }
        }
        self.check_failed();
    }

    /// Returns whether the remote node connection has failed.
    fn node_status(&self) -> Result<(), NodeError> {
        if !self.failed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.node_error.lock().clone()
    }
}

/// The interior state of a port.
#[derive(Debug)]
struct PortInner {
    id: PortId,
    state: Mutex<PortInnerState>,
}

/// A control object used by [`HandlePortEvent`] operations.
pub struct PortControl<'a, 'm> {
    peer_and_seq: Option<(&'a PortRef, &'a mut Seq)>,
    events: &'a mut PendingEvents<'m>,
}

impl<'a, 'm> PortControl<'a, 'm> {
    fn peered(peer: &'a PortRef, seq: &'a mut Seq, events: &'a mut PendingEvents<'m>) -> Self {
        Self {
            peer_and_seq: Some((peer, seq)),
            events,
        }
    }

    fn unpeered(events: &'a mut PendingEvents<'m>) -> Self {
        Self {
            peer_and_seq: None,
            events,
        }
    }

    /// Sends a message to the peer port.
    pub fn respond(&mut self, message: Message<'m>) {
        if let Some((port_ref, seq)) = &mut self.peer_and_seq {
            let this = **seq;
            **seq += Wrapping(1);
            self.events.push(
                port_ref.clone(),
                this,
                PortEvent::Message {
                    message: Some(message),
                    close: false,
                },
            )
        }
    }

    /// Defers a waker to be awoken outside the port lock.
    pub fn wake(&mut self, waker: Waker) {
        self.events.wake(waker);
    }
}

/// Trait implemented by port event handlers.
///
/// Such an implementation can be associated with a port by calling
/// [`Port::set_handler`].
pub trait HandlePortEvent: 'static + Send {
    /// Handles a new message for the port.
    ///
    /// If an error is returned, the port will be failed (and the caller will
    /// call the `fail` method).
    fn message<'a>(
        &mut self,
        control: &mut PortControl<'_, 'a>,
        message: Message<'a>,
    ) -> Result<(), HandleMessageError>;

    /// Handles the port closing.
    fn close(&mut self, control: &mut PortControl<'_, '_>);

    /// Handles a port failure.
    fn fail(&mut self, control: &mut PortControl<'_, '_>, err: NodeError);

    /// Returns all unconsumed messages.
    ///
    /// This is used when the handler is being released, such as when sending
    /// the port to another node.
    fn drain(&mut self) -> Vec<OwnedMessage>;
}

/// Error returned by [`HandlePortEvent::message`] when the message is invalid
/// or the port should otherwise be failed.
pub struct HandleMessageError(Box<dyn std::error::Error + Send + Sync>);

impl HandleMessageError {
    /// Creates a new error.
    pub fn new<E: Into<Box<dyn std::error::Error + Send + Sync>>>(err: E) -> Self {
        Self(err.into())
    }
}

/// An error that occurred communicating with another node.
#[derive(Clone, Debug, Error)]
#[error(transparent)]
pub struct NodeError(Arc<NodeErrorInner>);

impl NodeError {
    fn new(node: &NodeId, source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(Arc::new(NodeErrorInner {
            node_id: Some(*node),
            source: source.into(),
        }))
    }

    fn local(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(Arc::new(NodeErrorInner {
            node_id: None,
            source: source.into(),
        }))
    }

    fn shutting_down() -> Self {
        Self::local(ShuttingDownError)
    }

    fn remote_node_id(&self) -> Option<&NodeId> {
        if let Some(err) = self.0.source.downcast_ref::<RemotePortError>() {
            Some(&err.0)
        } else {
            self.0.node_id.as_ref()
        }
    }

    fn node_id(&self) -> Option<&NodeId> {
        self.0.node_id.as_ref()
    }
}

#[derive(Debug, Error)]
struct NodeErrorInner {
    node_id: Option<NodeId>,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl Display for NodeErrorInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(node_id) = self.node_id {
            write!(f, "communication with node {node_id:?} failed")
        } else {
            write!(f, "local mesh failure")
        }
    }
}

#[derive(Debug, Error)]
#[error("mesh is shutting down")]
struct ShuttingDownError;

#[derive(Debug, Error)]
#[error("received unknown local port")]
struct UnknownLocalPort;

#[derive(Debug, Error)]
#[error("port failed on remote node due to node {0:?}")]
struct RemotePortError(NodeId);

#[derive(Debug, Error)]
#[error("remote node disconnected")]
struct RemoteNodeDisconnected;

#[derive(Debug, Error)]
#[error("remote node dropped")]
struct RemoteNodeDropped;

trait HandlePortEventAndAny: HandlePortEvent + Any {}

impl Debug for dyn HandlePortEventAndAny {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("HandlePortEvent")
    }
}

impl<T: HandlePortEvent> HandlePortEventAndAny for T {}

/// The mutable interior state of a port.
#[derive(Debug)]
struct PortInnerState {
    activity: PortActivity,
    local_node: Option<Weak<LocalNodeInner>>,

    event_queue: EventQueue,
    handler: Option<Box<dyn HandlePortEventAndAny>>,

    next_local_seq: Seq,
    is_local_closed: bool,
}

/// A [`HandlePortEvent`] implementation that just queues the messages.
///
/// This is used when no other handler is registered.
#[derive(Default)]
struct QueuingHandler {
    messages: Vec<OwnedMessage>,
}

impl HandlePortEvent for QueuingHandler {
    fn message(
        &mut self,
        _control: &mut PortControl<'_, '_>,
        message: Message<'_>,
    ) -> Result<(), HandleMessageError> {
        self.messages.push(message.into_owned());
        Ok(())
    }

    fn close(&mut self, _control: &mut PortControl<'_, '_>) {}

    fn fail(&mut self, _control: &mut PortControl<'_, '_>, _err: NodeError) {}

    fn drain(&mut self) -> Vec<OwnedMessage> {
        std::mem::take(&mut self.messages)
    }
}

#[derive(Debug)]
struct EventQueue {
    next_peer_seq: Seq,
    heap: BinaryHeap<Reverse<SeqValue<OwnedPortEvent>>>,
}

impl EventQueue {
    fn new() -> Self {
        Self {
            next_peer_seq: Wrapping(1),
            heap: BinaryHeap::new(),
        }
    }

    /// Pops the next event from the event queue.
    ///
    /// If `v` is `Some`, it is logically added to the queue first.
    /// If it is the next event, it is instead returned directly.
    fn pop<'a>(&mut self, v: Option<(Seq, PortEvent<'a>)>) -> Option<PortEvent<'a>> {
        if let Some((seq, event)) = v {
            if seq == self.next_peer_seq {
                self.next_peer_seq += Wrapping(1);
                return Some(event);
            }
            self.add(seq, event);
        }
        if let Some(Reverse(SeqValue(seq, _))) = self.heap.peek() {
            if *seq > self.next_peer_seq {
                return None;
            }
            let Reverse(SeqValue(_, port_event)) = self.heap.pop().unwrap();
            self.next_peer_seq += Wrapping(1);
            return Some(port_event.into());
        }
        None
    }

    fn add(&mut self, seq: Seq, event: PortEvent<'_>) {
        assert!(seq >= self.next_peer_seq);
        self.heap.push(Reverse(SeqValue(seq, event.into_owned())));
    }

    fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

/// The port activity state.
#[derive(Clone, Debug)]
enum PortActivity {
    Peered(PortRef),
    Sending { peer: PortRef, target: PortRef },
    Proxying { peer: PortRef, target: PortRef },
    Failed(NodeError),
    Done,
    Unreachable,
}

/// A reference to a local or remote port.
#[derive(Clone)]
enum PortRef {
    LocalPort(Arc<PortInner>),
    RemotePort(Arc<RemoteNode>, PortId),
}

impl Debug for PortRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortRef::LocalPort(port) => f.debug_tuple("LocalPort").field(&port.id).finish(),
            PortRef::RemotePort(remote_node, port_id) => f
                .debug_tuple("RemotePort")
                .field(&remote_node.id)
                .field(&port_id)
                .finish(),
        }
    }
}

impl PortRef {
    fn node_status(&self) -> Result<(), NodeError> {
        match self {
            PortRef::LocalPort(_) => Ok(()),
            PortRef::RemotePort(node, _) => node.node_status(),
        }
    }

    /// Returns whether messages to this port can reference `port`.
    fn is_compatible_node(&self, local_node: &Option<Weak<LocalNodeInner>>) -> bool {
        match local_node {
            None => true,
            Some(local_node) => match self {
                PortRef::LocalPort(_) => true,
                PortRef::RemotePort(node, _) => {
                    Weak::as_ptr(local_node) == Arc::as_ptr(&node.local_node)
                }
            },
        }
    }
}

impl PortInnerState {
    fn new(activity: PortActivity) -> Self {
        Self {
            local_node: None,
            activity,
            next_local_seq: Wrapping(1),
            event_queue: EventQueue::new(),
            handler: None,
            is_local_closed: false,
        }
    }

    /// Fails a port, notifying any nodes that might be interested.
    fn fail(&mut self, pending_events: &mut PendingEvents<'_>, err: NodeError) {
        match std::mem::replace(&mut self.activity, PortActivity::Failed(err.clone())) {
            PortActivity::Peered(peer) => {
                pending_events.push(peer, Wrapping(0), NonMessageEvent::FailPort(err));
            }
            PortActivity::Sending { peer, target } | PortActivity::Proxying { peer, target } => {
                pending_events.push(peer, Wrapping(0), NonMessageEvent::FailPort(err.clone()));
                pending_events.push(target, Wrapping(0), NonMessageEvent::FailPort(err.clone()));
            }
            activity @ PortActivity::Failed(_) => {
                // Put the old error back.
                self.activity = activity;
            }
            PortActivity::Done => {}
            PortActivity::Unreachable => unreachable!(),
        }
    }

    fn set_activity(&mut self, activity: PortActivity) {
        self.activity = activity;
    }

    /// Returns the peer and sequence number for the next outgoing event.
    /// Returns None if the port is not peered.
    fn next_peer_and_seq(&mut self) -> Option<(PortRef, Seq)> {
        match &self.activity {
            PortActivity::Peered(peer) => {
                let peer = peer.clone();
                let seq = self.next_local_seq;
                self.next_local_seq += Wrapping(1);
                Some((peer, seq))
            }
            PortActivity::Done | PortActivity::Failed(_) => None, // TODO: fail ports in message
            s => unreachable!("{:?}", s),
        }
    }
}

/// Protocol error for node events.
#[derive(Debug)]
enum EventError {
    UnknownPort,
    Truncated,
    // Field is stored solely for logging via debug, not actually dead.
    UnknownEventType(#[expect(dead_code)] protocol::EventType),
    MissingOsResource,
}

/// A list of pending local and remote events to send. This is used to avoid
/// sending events recursively or under locks.
struct PendingEvents<'a> {
    local_events: VecDeque<(Arc<PortInner>, Seq, PortEvent<'a>)>,
    remote_events: Vec<(Arc<RemoteNode>, PortId, Seq, PortEvent<'a>)>,
    wakers: Vec<Waker>,
}

impl<'a> PendingEvents<'a> {
    fn new() -> Self {
        Self {
            local_events: VecDeque::new(),
            remote_events: Vec::new(),
            wakers: Vec::new(),
        }
    }

    /// Sends an event to a local port, then sends any events generated by this
    /// operation.
    fn send_local(
        port: &Arc<PortInner>,
        remote_node_id: Option<&NodeId>,
        seq: Seq,
        event: PortEvent<'a>,
    ) {
        let mut this = Self::new();
        port.on_event(remote_node_id, seq, event, &mut this);
        this.process();
    }

    /// Sends an event to a port, then sends any events generated by this
    /// operation.
    fn send(port: &PortRef, seq: Seq, event: impl Into<PortEvent<'a>>) {
        let event = event.into();
        match port {
            PortRef::LocalPort(port) => Self::send_local(port, None, seq, event),
            PortRef::RemotePort(remote_node, port_id) => {
                remote_node.event(*port_id, seq, event);
            }
        }
    }

    /// Sends pending events until there are no more to send.
    fn process(mut self) {
        while let Some((port, seq, event)) = self.local_events.pop_front() {
            port.on_event(None, seq, event, &mut self);
        }
        for (remote_node, port_id, seq, event) in self.remote_events.drain(..) {
            remote_node.event(port_id, seq, event);
        }
        for waker in self.wakers {
            waker.wake();
        }
    }

    /// Pushes an event targeting a local port to the event list.
    fn push_local(&mut self, port: Arc<PortInner>, seq: Seq, event: PortEvent<'a>) {
        self.local_events.push_back((port, seq, event));
    }

    /// Pushes an event to the event list.
    fn push(&mut self, port: PortRef, seq: Seq, event: impl Into<PortEvent<'a>>) {
        let event = event.into();
        match port {
            PortRef::LocalPort(port) => self.push_local(port, seq, event),
            PortRef::RemotePort(remote_node, port_id) => {
                self.remote_events.push((remote_node, port_id, seq, event));
            }
        }
    }

    fn wake(&mut self, waker: Waker) {
        self.wakers.push(waker);
    }
}

/// Protocol error for port events.
#[derive(Debug, Error)]
enum PortError {
    #[error("duplicate sequence number")]
    DuplicateSeq { next: Seq },
    #[error("received event after port closed")]
    EventAfterClose,
    #[error("unexpected acknowledgement of peer change")]
    AckChangePeerInvalidState,
    #[error("received event after proxy end")]
    EventAfterProxyEnd,
    #[error("circular bridge")]
    CircularBridge,
    #[error("invalid state for proxy")]
    InvalidStateForProxy,
    #[error("failed to parse message")]
    BadMessage(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// The result of a port event operation.
enum PortEventResult {
    /// No change to the port.
    None,
    /// There will be no more cross-node communication for this port.
    /// Disassociate the port from the node.
    Done,
}

impl PortInnerState {
    /// Handles an incoming port event.
    fn on_event<'a>(
        &mut self,
        remote_node_id: Option<&NodeId>,
        seq: Seq,
        event: PortEvent<'a>,
        pending_events: &mut PendingEvents<'a>,
    ) -> Result<PortEventResult, NodeError> {
        if let PortEvent::Event(NonMessageEvent::FailPort(err)) = event {
            return Err(err);
        }

        let err = 'error: {
            if seq < self.event_queue.next_peer_seq {
                break 'error PortError::DuplicateSeq {
                    next: self.event_queue.next_peer_seq,
                };
            }

            match &mut self.activity {
                PortActivity::Peered(peer) => {
                    let mut v = Some((seq, event));
                    while let Some(port_event) = self.event_queue.pop(v.take()) {
                        match port_event {
                            PortEvent::Message { message, close } => {
                                if let Some(message) = message {
                                    let handler = self
                                        .handler
                                        .get_or_insert_with(|| Box::new(QueuingHandler::default()));
                                    if let Err(err) = handler.message(
                                        &mut PortControl::peered(
                                            peer,
                                            &mut self.next_local_seq,
                                            pending_events,
                                        ),
                                        message,
                                    ) {
                                        break 'error PortError::BadMessage(err.0);
                                    }
                                }
                                if close {
                                    if !self.event_queue.is_empty() {
                                        break 'error PortError::EventAfterClose;
                                    }
                                    if !self.is_local_closed {
                                        pending_events.push(
                                            peer.clone(),
                                            self.next_local_seq,
                                            PortEvent::Message {
                                                message: None,
                                                close: true,
                                            },
                                        );
                                    }
                                    return Ok(PortEventResult::Done);
                                }
                            }
                            PortEvent::Event(e) => match e {
                                NonMessageEvent::ChangePeer(new_peer, seq_delta) => {
                                    assert!(new_peer.is_compatible_node(&self.local_node));
                                    new_peer.node_status()?;
                                    let old_peer = std::mem::replace(peer, new_peer);
                                    pending_events.push(
                                        old_peer,
                                        self.next_local_seq,
                                        NonMessageEvent::AcknowledgeChangePeer,
                                    );
                                    self.next_local_seq -= seq_delta;
                                }
                                NonMessageEvent::AcknowledgeChangePeer => {
                                    break 'error PortError::AckChangePeerInvalidState;
                                }
                                NonMessageEvent::AcknowledgePort | NonMessageEvent::FailPort(_) => {
                                    unreachable!()
                                }
                            },
                        }
                    }
                    return Ok(PortEventResult::None);
                }
                PortActivity::Sending { .. } => {
                    // Save but do not process the events since they'll be
                    // forwarded.
                    self.event_queue.add(seq, event);
                    return Ok(PortEventResult::None);
                }
                PortActivity::Proxying { peer: _, target } => {
                    let target = target.clone();

                    let mut v = Some((seq, event));
                    let mut next_seq = self.next_local_seq;
                    while let Some(port_event) = self.event_queue.pop(v.take()) {
                        match port_event {
                            PortEvent::Event(NonMessageEvent::AcknowledgeChangePeer) => {
                                if !self.event_queue.is_empty() {
                                    break 'error PortError::EventAfterProxyEnd;
                                }
                                return Ok(PortEventResult::Done);
                            }
                            event => {
                                if let PortEvent::Event(NonMessageEvent::ChangePeer(new_peer, _)) =
                                    &event
                                {
                                    assert!(new_peer.is_compatible_node(&self.local_node));
                                    new_peer.node_status()?;
                                    self.set_activity(PortActivity::Proxying {
                                        peer: new_peer.clone(),
                                        target: target.clone(),
                                    });
                                }
                                pending_events.push(target.clone(), next_seq, event);
                                next_seq += Wrapping(1);
                            }
                        }
                    }

                    self.next_local_seq = next_seq;
                    return Ok(PortEventResult::None);
                }
                PortActivity::Done => PortError::EventAfterClose,
                PortActivity::Failed(err) => return Err(err.clone()),
                PortActivity::Unreachable => unreachable!(),
            }
        };
        if let Some(remote_node_id) = remote_node_id {
            Err(NodeError::new(remote_node_id, err))
        } else {
            Err(NodeError::local(err))
        }
    }

    /// Starts proxying incoming events.
    fn start_proxy(
        &mut self,
        peer: PortRef,
        target: PortRef,
        initial_seq: Seq,
        pending_events: &mut PendingEvents<'_>,
    ) {
        let mut seq = initial_seq;

        // Send any messages in the queue.
        if let Some(handler) = &mut self.handler {
            for message in handler.drain() {
                pending_events.push(
                    target.clone(),
                    seq,
                    OwnedPortEvent::Message {
                        message: Some(message),
                        close: false,
                    },
                );
                seq += Wrapping(1);
            }
        }

        // Send the event queue.
        while let Some(port_event) = self.event_queue.pop(None) {
            pending_events.push(target.clone(), seq, port_event);
            seq += Wrapping(1);
        }

        let change_seq = self.next_local_seq;

        self.next_local_seq = seq;
        let delta = self.event_queue.next_peer_seq - self.next_local_seq;
        self.set_activity(PortActivity::Proxying {
            peer: peer.clone(),
            target: target.clone(),
        });

        pending_events.push(peer, change_seq, NonMessageEvent::ChangePeer(target, delta));
    }
}

impl PortInner {
    /// Sends a message on the port.
    fn send(&self, message: Message<'_>) {
        let peer_seq = {
            let mut state = self.state.lock();
            assert!(!state.is_local_closed);
            state.next_peer_and_seq()
        };

        if let Some((peer, seq)) = peer_seq {
            PendingEvents::send(
                &peer,
                seq,
                PortEvent::Message {
                    message: Some(message),
                    close: false,
                },
            );
        }
    }

    /// Closes the port, optionally sending a message first. After this, no
    /// messages may be sent or received.
    fn close(&self, message: Option<Message<'_>>) {
        let _old_handler;
        let peer_seq = {
            let mut state = self.state.lock();
            assert!(!state.is_local_closed);

            // Clear the handler now so that it is reliably dropped.
            _old_handler = std::mem::take(&mut state.handler);

            state.is_local_closed = true;
            state.next_peer_and_seq()
        };

        if let Some((peer, seq)) = peer_seq {
            PendingEvents::send(
                &peer,
                seq,
                PortEvent::Message {
                    message,
                    close: true,
                },
            );
        }
    }

    /// Handles an incoming event.
    fn on_event<'a>(
        &self,
        remote_node_id: Option<&NodeId>,
        seq: Seq,
        event: PortEvent<'a>,
        pending_events: &mut PendingEvents<'a>,
    ) {
        let mut state = self.state.lock();
        let mut disassociate = false;
        match state.on_event(remote_node_id, seq, event, pending_events) {
            Ok(PortEventResult::None) => {}
            Ok(PortEventResult::Done) => {
                state.set_activity(PortActivity::Done);
                if let Some(handler) = &mut state.handler {
                    handler.close(&mut PortControl::unpeered(pending_events));
                }
                disassociate = true;
            }
            Err(err) => {
                state.fail(pending_events, err.clone());
                if let Some(handler) = &mut state.handler {
                    handler.fail(&mut PortControl::unpeered(pending_events), err);
                }
                disassociate = true;
            }
        }

        if disassociate {
            self.disassociate(&mut state);
        }
        drop(state);
    }

    /// Starts proxying incoming events.
    fn start_proxy(
        &self,
        remote_node_id: &NodeId,
        initial_seq: Seq,
        pending_events: &mut PendingEvents<'_>,
    ) {
        tracing::trace!(port = ?self.id, initial_seq, "proxy starting");
        let mut state = self.state.lock();

        let mut err = None;
        match std::mem::replace(&mut state.activity, PortActivity::Unreachable) {
            PortActivity::Sending { peer, target } => {
                state.start_proxy(peer, target, initial_seq, pending_events);
            }
            activity => {
                state.activity = activity;
                err = Some(NodeError::new(
                    remote_node_id,
                    PortError::InvalidStateForProxy,
                ));
            }
        };

        if let Some(err) = err {
            self.disassociate(&mut state);
            if let Some(handler) = &mut state.handler {
                handler.fail(
                    &mut PortControl::unpeered(pending_events),
                    NodeError::new(remote_node_id, err),
                );
            }

            drop(state);
            // Trace outside the lock to avoid deadlocks.
            tracing::error!(port = ?self.id, "proxy from wrong state");
        }
    }

    /// Associates the port with a given local node.
    ///
    /// Panics if the port is already associated with a different node.
    fn associate<'a>(
        inner: &'a Arc<Self>,
        local_node: &Arc<LocalNodeInner>,
    ) -> MutexGuard<'a, PortInnerState> {
        let mut state = inner.state.lock();
        match &state.local_node {
            Some(node) => assert_eq!(Arc::as_ptr(local_node), node.as_ptr()),
            None => {
                local_node
                    .state
                    .lock()
                    .ports
                    .insert(inner.id, inner.clone());
                state.local_node = Some(Arc::downgrade(local_node));
            }
        }
        state
    }

    /// Disassociates the port with its local node.
    fn disassociate(&self, port_state: &mut PortInnerState) {
        if let Some(local_node) = port_state
            .local_node
            .take()
            .as_ref()
            .and_then(Weak::upgrade)
        {
            tracing::trace!(node = ?local_node.id, port = ?self.id, "disassociate port");
            let mut state = local_node.state.lock();
            state.ports.remove(&self.id);
            let shutdown = state.shutdown.take();
            drop(state);
            // Trace outside the lock to avoid deadlocks.
            if let Some(shutdown) = shutdown {
                tracing::trace!(node = ?local_node.id, "waking shutdown waiter");
                shutdown.close();
            }
        }
    }

    /// Lock two ports' states, carefully taking the locks in a consistent order
    /// to avoid deadlocks.
    fn lock_two<'a>(
        left: &'a Self,
        right: &'a Self,
    ) -> (
        MutexGuard<'a, PortInnerState>,
        MutexGuard<'a, PortInnerState>,
    ) {
        // N.B. For the same two locks passed to this function their memory
        //      addresses will be the same but the order of the arguments may
        //      differ.
        let (lm, rm);
        if std::ptr::from_ref(left) < std::ptr::from_ref(right) {
            lm = left.state.lock();
            rm = right.state.lock();
        } else {
            rm = right.state.lock();
            lm = left.state.lock();
        }
        (lm, rm)
    }

    fn set_handler(&self, mut handler: Box<dyn HandlePortEventAndAny>) {
        let mut pending_events = PendingEvents::new();
        {
            let mut state = self.state.lock();
            let state = &mut *state;
            let peer_and_seq = match &state.activity {
                PortActivity::Peered(peer) => Some((peer, &mut state.next_local_seq)),
                _ => None,
            };
            let mut control = PortControl {
                peer_and_seq,
                events: &mut pending_events,
            };
            if let Some(mut old_handler) = state.handler.take() {
                for message in old_handler.drain() {
                    if let Err(err) = handler.message(&mut control, message.into()) {
                        state.fail(
                            &mut pending_events,
                            NodeError::local(PortError::BadMessage(err.0)),
                        );
                        break;
                    }
                }
            }
            match &state.activity {
                PortActivity::Peered(_) => {}
                PortActivity::Failed(err) => {
                    handler.fail(&mut PortControl::unpeered(&mut pending_events), err.clone())
                }
                PortActivity::Done => {
                    handler.close(&mut PortControl::unpeered(&mut pending_events))
                }
                _ => unreachable!(),
            }
            state.handler = Some(handler);
        }
        pending_events.process();
    }

    fn drain_queue(&self) -> Option<Box<dyn HandlePortEventAndAny>> {
        let mut state = self.state.lock();
        let mut handler = state.handler.take();
        let messages = handler
            .as_mut()
            .map_or_else(Vec::new, |handler| handler.drain());
        if !messages.is_empty() {
            state.handler = Some(Box::new(QueuingHandler { messages }));
        }
        handler
    }
}

/// A handle to a remote node connection. When dropped, the connection is
/// failed, along with any associated ports.
pub struct RemoteNodeHandle {
    id: NodeId,
    remote_node: Weak<RemoteNode>,
}

impl Debug for RemoteNodeHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteNodeHandle")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for RemoteNodeHandle {
    fn drop(&mut self) {
        if let Some(remote_node) = self.remote_node.upgrade() {
            remote_node.local_node.drop_remote_handle(&remote_node);
        }
    }
}

impl RemoteNodeHandle {
    pub fn id(&self) -> &NodeId {
        &self.id
    }

    /// Establishes the remote node connection.
    pub fn connect<T: 'static + SendEvent>(&self, conn: T) -> bool {
        if let Some(remote_node) = self.remote_node.upgrade() {
            remote_node.connect(Box::new(conn))
        } else {
            false
        }
    }

    pub fn disconnect(&self) {
        self.fail(RemoteNodeDisconnected)
    }

    pub fn fail(&self, err: impl Into<Box<dyn std::error::Error + Send + Sync>>) {
        if let Some(remote_node) = self.remote_node.upgrade() {
            remote_node
                .local_node
                .disconnect_remote(&remote_node, NodeError::new(&remote_node.id, err));
        }
    }
}

impl Clone for RemoteNodeHandle {
    fn clone(&self) -> Self {
        if let Some(remote_node) = self.remote_node.upgrade() {
            assert!(remote_node.handle_count.fetch_add(1, Ordering::SeqCst) > 0);
        }
        Self {
            id: self.id,
            remote_node: self.remote_node.clone(),
        }
    }
}

/// The mutable interior state for [`LocalNode`].
#[derive(Debug)]
struct LocalNodeState {
    ports: HashMap<PortId, Arc<PortInner>>,
    nodes: HashMap<NodeId, Arc<RemoteNode>>,
    shutdown: Option<async_channel::Sender<()>>,
}

/// The deserialized event for processing by a local port.
#[derive(Debug)]
enum PortEvent<'a> {
    Message {
        message: Option<Message<'a>>,
        close: bool,
    },
    Event(NonMessageEvent),
}

impl From<NonMessageEvent> for PortEvent<'_> {
    fn from(value: NonMessageEvent) -> Self {
        PortEvent::Event(value)
    }
}

impl From<OwnedPortEvent> for PortEvent<'_> {
    fn from(value: OwnedPortEvent) -> Self {
        match value {
            OwnedPortEvent::Message { message, close } => PortEvent::Message {
                message: message.map(Into::into),
                close,
            },
            OwnedPortEvent::Event(e) => PortEvent::Event(e),
        }
    }
}

impl PortEvent<'_> {
    fn into_owned(self) -> OwnedPortEvent {
        match self {
            PortEvent::Message { message, close } => OwnedPortEvent::Message {
                message: message.map(|m| m.into_owned()),
                close,
            },
            PortEvent::Event(event) => OwnedPortEvent::Event(event),
        }
    }
}

/// An owning version of [`PortEvent`].
#[derive(Debug)]
enum OwnedPortEvent {
    Message {
        message: Option<OwnedMessage>,
        close: bool,
    },
    Event(NonMessageEvent),
}

/// A port event exclusive of a message event.
#[derive(Debug)]
enum NonMessageEvent {
    ChangePeer(PortRef, Seq),
    AcknowledgeChangePeer,
    AcknowledgePort,
    FailPort(NodeError),
}

/// An event to be sent to a remote node.
pub struct OutgoingEvent<'a> {
    port_id: PortId,
    seq: Seq,
    event: EventAndEncoder<'a>,
    len: usize,
    remote_node: &'a Arc<RemoteNode>,
}

enum EventAndEncoder<'a> {
    Message {
        message: Option<Encoder<Message<'a>, <Message<'a> as DefaultEncoding>::Encoding, Resource>>,
        close: bool,
    },
    Other(NonMessageEvent),
}

impl<'a> OutgoingEvent<'a> {
    fn new(
        port_id: PortId,
        seq: Seq,
        event: PortEvent<'a>,
        remote_node: &'a Arc<RemoteNode>,
    ) -> Self {
        let mut len = size_of::<protocol::Event>();
        let event = match event {
            PortEvent::Message { message, close } => {
                let message = message.map(|m| {
                    let message = Encoder::new(m);
                    len += message.resource_count() * size_of::<protocol::ResourceData>();
                    len += message.len();
                    message
                });
                EventAndEncoder::Message { message, close }
            }
            PortEvent::Event(event) => match event {
                NonMessageEvent::ChangePeer(_, _) => {
                    len += size_of::<protocol::ChangePeerData>();
                    EventAndEncoder::Other(event)
                }
                NonMessageEvent::FailPort(_) => {
                    len += size_of::<protocol::FailPortData>();
                    EventAndEncoder::Other(event)
                }
                event @ (NonMessageEvent::AcknowledgeChangePeer
                | NonMessageEvent::AcknowledgePort) => EventAndEncoder::Other(event),
            },
        };
        Self {
            port_id,
            seq,
            event,
            len,
            remote_node,
        }
    }

    /// The size of the event in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Serializes the event to `buf`, adding the OS resources to `os_resources`.
    pub fn write_to(self, buf: &mut dyn Buffer, os_resources: &mut impl Extend<OsResource>) {
        write_with(buf, |mut buf| {
            buf.write_split(size_of::<protocol::Event>(), |header_buf, buf| {
                self.write_split(header_buf, buf, os_resources);
            })
        })
    }

    fn write_split(
        self,
        mut header_buf: Buf<'_>,
        mut buf: Buf<'_>,
        os_resources: &mut impl Extend<OsResource>,
    ) {
        let mut header = protocol::Event {
            port_id: self.port_id.0.into(),
            seq: self.seq.0,
            ..protocol::Event::new_zeroed()
        };
        match self.event {
            EventAndEncoder::Other(event) => match event {
                NonMessageEvent::ChangePeer(port, seq_delta) => {
                    let (node_id, port_id) = match port {
                        PortRef::LocalPort(port) => {
                            drop(PortInner::associate(&port, &self.remote_node.local_node));
                            (self.remote_node.local_node.id, port.id)
                        }
                        PortRef::RemotePort(remote_node, port_id) => (remote_node.id, port_id),
                    };
                    header.event_type = protocol::EventType::CHANGE_PEER;
                    header.message_size = size_of::<protocol::ChangePeerData>() as u32;
                    buf.append(
                        protocol::ChangePeerData {
                            node: node_id.0.into(),
                            port: port_id.0.into(),
                            seq_delta: seq_delta.0,
                            reserved: 0,
                        }
                        .as_bytes(),
                    );
                }
                NonMessageEvent::AcknowledgeChangePeer => {
                    header.event_type = protocol::EventType::ACKNOWLEDGE_CHANGE_PEER
                }
                NonMessageEvent::AcknowledgePort => {
                    header.event_type = protocol::EventType::ACKNOWLEDGE_PORT
                }
                NonMessageEvent::FailPort(err) => {
                    header.event_type = protocol::EventType::FAIL_PORT;
                    header.message_size = size_of::<protocol::FailPortData>() as u32;
                    buf.append(
                        protocol::FailPortData {
                            node: err
                                .remote_node_id()
                                .unwrap_or(&self.remote_node.local_node.id)
                                .0
                                .into(),
                        }
                        .as_bytes(),
                    );
                }
            },
            EventAndEncoder::Message { message, close } => {
                let mut resources = Vec::new();
                header.event_type = protocol::EventType::MESSAGE;
                header.flags.set_close(close);
                if let Some(message) = message {
                    header.flags.set_message(true);
                    header.message_size = message.len() as u32;
                    header.resource_count = message.resource_count() as u32;
                    buf.write_split(
                        message.resource_count() * size_of::<protocol::ResourceData>(),
                        |mut resource_buf, mut message_buf| {
                            message.encode_into(&mut message_buf, &mut resources);
                            for resource in resources {
                                let data = match resource {
                                    Resource::Port(port) => port.prepare_to_send(self.remote_node),
                                    Resource::Os(r) => {
                                        os_resources.extend([r]);
                                        protocol::ResourceData::new_zeroed()
                                    }
                                };
                                resource_buf.append(data.as_bytes());
                            }
                        },
                    );
                }
            }
        }

        // Write the header.
        header_buf.append(header.as_bytes());
    }
}

/// Trait for sending events to a remote node.
pub trait SendEvent: Send + Sync {
    fn event(&self, event: OutgoingEvent<'_>);
}

/// Trait for establishing a connection to a remote node.
pub trait Connect: Send + Sync {
    fn connect(&self, node_id: NodeId, handle: RemoteNodeHandle);
}

impl LocalNode {
    /// Creates a new node with `node_id`, using `connector` to establish
    /// connections to remote nodes.
    pub fn with_id(node_id: NodeId, connector: Box<dyn Connect>) -> Self {
        let node = Arc::new(LocalNodeInner {
            id: node_id,
            state: Mutex::new(LocalNodeState {
                ports: HashMap::new(),
                nodes: HashMap::new(),
                shutdown: None,
            }),
        });
        Self {
            inner: node,
            connector: Mutex::new(Some(connector)),
        }
    }

    /// The node's ID.
    pub fn id(&self) -> NodeId {
        self.inner.id
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.inner.state.lock().ports.is_empty()
    }

    /// Waits for all ports to be disassociated from the node.
    ///
    /// If `all_ports` is false, only waits for ports that are still in the
    /// process of being sent to another node.
    pub async fn wait_for_ports(&self, all_ports: bool) {
        loop {
            let (send, recv) = async_channel::bounded::<()>(1);
            let ports: Vec<_> = {
                let mut state = self.inner.state.lock();
                state.shutdown = Some(send);
                state.ports.values().cloned().collect()
            };
            let left = ports
                .into_iter()
                .filter(|port| {
                    let wait = all_ports
                        || match &port.state.lock().activity {
                            PortActivity::Peered(_) => false,
                            PortActivity::Sending { .. } => true,
                            PortActivity::Proxying { .. } => true,
                            PortActivity::Failed(_) => false,
                            PortActivity::Done => false,
                            PortActivity::Unreachable => unreachable!(),
                        };
                    if wait {
                        tracing::trace!(node = ?self.id(), ?port, "waiting for port");
                    }
                    wait
                })
                .count();
            if left == 0 {
                tracing::trace!(node = ?self.id(), "no ports remain");
                return;
            }
            tracing::trace!(node = ?self.id(), count = left, "waiting for ports");
            let _ = recv.recv().await;
        }
    }

    pub fn drop_connector(&self) {
        self.connector.lock().take();
    }

    pub fn fail_all_nodes(&self) {
        // Prevent new connections.
        self.drop_connector();
        self.inner.fail_all_nodes(NodeError::shutting_down());
    }

    pub fn add_port(&self, id: PortId, peer: Address) -> Port {
        tracing::trace!(node = ?self.inner.id, port = ?id, peer = ?peer, "importing port");
        let peer_node = self.get_remote(peer.node);
        let activity = PortActivity::Peered(PortRef::RemotePort(peer_node.clone(), peer.port));

        let port = Port::new(id, PortInnerState::new(activity));
        {
            let mut state = PortInner::associate(&port.inner, &self.inner);
            if let Err(err) = peer_node.node_status() {
                state.set_activity(PortActivity::Failed(err));
                port.inner.disassociate(&mut state);
            }
        }
        port
    }

    /// Adds a new remote node.
    pub fn add_remote(&self, id: NodeId) -> RemoteNodeHandle {
        let (deferred_conn, handle) = RemoteNode::new(self.inner.clone(), id);
        self.inner.state.lock().nodes.insert(id, deferred_conn);

        handle
    }

    /// Retrieves a handle to a remote node. When the last handle is dropped,
    /// the node will be disconnected.
    pub fn get_remote_handle(&self, id: NodeId) -> RemoteNodeHandle {
        let remote = self.get_remote(id);
        let handle = remote.handle_count.fetch_add(1, Ordering::SeqCst);
        assert!(handle >= 0);
        RemoteNodeHandle {
            id,
            remote_node: Arc::downgrade(&remote),
        }
    }

    /// Processes a node event.
    pub fn event(&self, remote_node_id: &NodeId, event: &[u8], os_resources: &mut Vec<OsResource>) {
        let parse = || {
            let header = protocol::Event::read_from_prefix(event).ok()?.0; // TODO: zerocopy: use-rest-of-range, option-to-error (https://github.com/microsoft/openvmm/issues/759)
            let (resources, message) = Ref::from_prefix_with_elems(
                &event[size_of_val(&header)..],
                header.resource_count as usize,
            )
            .ok()?; // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
            let message = message.get(..header.message_size as usize)?;
            Some((header, resources, message))
        };

        match parse() {
            Some((header, resources, message)) => {
                if let Err(error) =
                    self.on_parsed_event(remote_node_id, &header, &resources, message, os_resources)
                {
                    tracing::error!(
                        node = ?self.inner.id,
                        port = ?PortId(header.port_id.into()),
                        seq = header.seq,
                        ?error,
                        "node event failure"
                    );
                }
            }
            None => {
                tracing::error!(
                    node = ?self.inner.id,
                    "node event parse failure"
                );
            }
        }
    }

    fn on_parsed_event(
        &self,
        remote_node_id: &NodeId,
        header: &protocol::Event,
        resource_data: &[Unalign<protocol::ResourceData>],
        message: &[u8],
        os_resources: &mut Vec<OsResource>,
    ) -> Result<(), EventError> {
        let port_id = PortId(header.port_id.into());
        let seq = Wrapping(header.seq);

        tracing::trace!(
            node = ?self.inner.id,
            port = ?port_id,
            seq,
            event_type = ?header.event_type,
            "port event"
        );
        let port = self
            .get_local_port(port_id)
            .ok_or(EventError::UnknownPort)?;
        let mut os_resources = os_resources.drain(..);

        let port_event = match header.event_type {
            protocol::EventType::MESSAGE => {
                let message = if header.flags.message() {
                    // Consume all the ports.
                    let mut resources = Vec::with_capacity(resource_data.len());
                    for data in resource_data {
                        let data = data.get();
                        let r = if data.id.is_zero() {
                            Resource::Os(os_resources.next().ok_or(EventError::MissingOsResource)?)
                        } else {
                            Resource::Port(self.receive_port(remote_node_id, data))
                        };
                        resources.push(r);
                    }
                    Some(Message::serialized(message, resources))
                } else {
                    None
                };
                PortEvent::Message {
                    message,
                    close: header.flags.close(),
                }
            }
            protocol::EventType::CHANGE_PEER => {
                let data = protocol::ChangePeerData::read_from_prefix(message)
                    .map_err(|_| EventError::Truncated)?
                    .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                let port = self
                    .get_port(Address {
                        node: NodeId(data.node.into()),
                        port: PortId(data.port.into()),
                    })
                    .ok_or(EventError::UnknownPort)?;
                NonMessageEvent::ChangePeer(port, Wrapping(data.seq_delta)).into()
            }
            protocol::EventType::ACKNOWLEDGE_CHANGE_PEER => {
                NonMessageEvent::AcknowledgeChangePeer.into()
            }
            protocol::EventType::ACKNOWLEDGE_PORT => {
                let mut events = PendingEvents::new();
                port.start_proxy(remote_node_id, Wrapping(1), &mut events);
                events.process();
                return Ok(());
            }
            protocol::EventType::FAIL_PORT => {
                let data = protocol::FailPortData::read_from_prefix(message)
                    .map_err(|_| EventError::Truncated)?
                    .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                NonMessageEvent::FailPort(NodeError::new(
                    remote_node_id,
                    RemotePortError(NodeId(data.node.into())),
                ))
                .into()
            }
            ty => return Err(EventError::UnknownEventType(ty)),
        };
        PendingEvents::send_local(&port, Some(remote_node_id), seq, port_event);
        Ok(())
    }

    /// Adds a port from port data.
    fn receive_port(&self, remote_node_id: &NodeId, data: protocol::ResourceData) -> Port {
        let old_address = Address {
            node: NodeId(data.old_node.into()),
            port: PortId(data.old_port.into()),
        };

        let peer_address = if !data.peer_port.is_zero() {
            Ok(Address {
                node: NodeId(data.peer_node.into()),
                port: PortId(data.peer_port.into()),
            })
        } else {
            Err(NodeError::new(
                remote_node_id,
                RemotePortError(NodeId(data.peer_node.into())),
            ))
        };

        tracing::trace!(
            node = ?self.inner.id,
            port = ?PortId(data.id.into()),
            old_address = ?old_address,
            peer = ?peer_address,
            "received port"
        );

        let peer;
        let activity = match peer_address.and_then(|addr| {
            self.get_port(addr)
                .ok_or_else(|| NodeError::new(remote_node_id, UnknownLocalPort))
        }) {
            Ok(peer_port) => {
                peer = Some(peer_port.clone());
                PortActivity::Peered(peer_port)
            }
            Err(err) => {
                tracing::warn!(
                    node = ?self.inner.id,
                    port = ?PortId(data.id.into()),
                    error = &err as &dyn std::error::Error,
                    old_address = ?old_address,
                    "received failed port",
                );
                peer = None;
                PortActivity::Failed(err)
            }
        };

        let port = Port::new(
            PortId(data.id.into()),
            PortInnerState {
                next_local_seq: Wrapping(data.next_local_seq),
                ..PortInnerState::new(activity)
            },
        );
        if let Some(peer) = peer {
            let mut state = PortInner::associate(&port.inner, &self.inner);
            let source = self.get_remote(old_address.node);
            if let Err(err) = peer.node_status().and_then(|()| source.node_status()) {
                state.set_activity(PortActivity::Failed(err));
                port.inner.disassociate(&mut state);
            } else {
                drop(state);
                source.event(
                    old_address.port,
                    Wrapping(0),
                    NonMessageEvent::AcknowledgePort.into(),
                );
            }
        }
        port
    }

    /// Gets or establishes a remote node for `id`.
    fn get_remote(&self, id: NodeId) -> Arc<RemoteNode> {
        assert!(id != self.id());
        let mut state = self.inner.state.lock();
        match state.nodes.entry(id) {
            hash_map::Entry::Occupied(entry) => entry.get().clone(),
            hash_map::Entry::Vacant(entry) => {
                let (remote_node, handle) = RemoteNode::new(self.inner.clone(), id);
                entry.insert(remote_node.clone());
                drop(state);
                let connector = self.connector.lock();
                if let Some(connector) = &*connector {
                    connector.connect(id, handle);
                }
                remote_node
            }
        }
    }

    /// Gets the local port with ID `port_id`.
    fn get_local_port(&self, port_id: PortId) -> Option<Arc<PortInner>> {
        self.inner.state.lock().ports.get(&port_id).cloned()
    }

    /// Gets a reference to a port with address `address`.
    fn get_port(&self, address: Address) -> Option<PortRef> {
        let peer = if address.node == self.inner.id {
            PortRef::LocalPort(self.get_local_port(address.port)?)
        } else {
            PortRef::RemotePort(self.get_remote(address.node), address.port)
        };
        Some(peer)
    }
}

impl LocalNodeInner {
    /// Fails all the remote nodes.
    fn fail_all_nodes(&self, err: NodeError) {
        let nodes = std::mem::take(&mut self.state.lock().nodes);
        for (_, node) in nodes {
            node.fail(err.clone());
        }
    }

    /// Fails all the ports.
    fn fail_all_ports(&self, err: NodeError) {
        let ports = std::mem::take(&mut self.state.lock().ports);
        let mut pending_events = PendingEvents::new();
        let mut control = PortControl::unpeered(&mut pending_events);
        for (_, port) in ports {
            let mut state = port.state.lock();
            if let Some(handler) = &mut state.handler {
                handler.fail(&mut control, err.clone());
            }
            state.local_node = None;
            state.set_activity(PortActivity::Failed(err.clone()));
        }
        pending_events.process();
    }

    fn drop_remote_handle(&self, remote_node: &Arc<RemoteNode>) {
        let count = remote_node.handle_count.fetch_sub(1, Ordering::SeqCst);
        assert!(count > 0);
        if count == 1 {
            self.disconnect_remote(
                remote_node,
                NodeError::new(&remote_node.id, RemoteNodeDropped),
            );
        }
    }

    /// Disconnects a remote node by ID, failing down any associated ports.
    fn disconnect_remote(&self, remote_node: &Arc<RemoteNode>, err: NodeError) {
        tracing::trace!(node = ?self.id, remote_node = ?remote_node.id, "disconnecting node");

        // Fail the node so that no new ports will reference it.
        remote_node.fail(err.clone());

        // Capture all the ports in order to fail the ones associated with this
        // node.
        let ports: Vec<_> = self.state.lock().ports.values().cloned().collect();

        let mut pending_events = PendingEvents::new();
        for port in ports {
            let mut state = port.state.lock();
            let fail = match &state.activity {
                PortActivity::Failed(_) => continue,
                PortActivity::Proxying {
                    target: PortRef::RemotePort(node, _),
                    ..
                }
                | PortActivity::Proxying {
                    peer: PortRef::RemotePort(node, _),
                    ..
                }
                | PortActivity::Peered(PortRef::RemotePort(node, _))
                | PortActivity::Sending {
                    peer: PortRef::RemotePort(node, _),
                    ..
                }
                | PortActivity::Sending {
                    target: PortRef::RemotePort(node, _),
                    ..
                } if node.id == remote_node.id => true,
                _ => false,
            };
            if fail {
                state.fail(&mut pending_events, err.clone());
                if let Some(handler) = &mut state.handler {
                    handler.fail(&mut PortControl::unpeered(&mut pending_events), err.clone());
                }
                port.disassociate(&mut state);
                drop(state);

                // Trace outside the lock to avoid deadlocks.
                tracing::trace!(
                    local_id = ?self.id,
                    port = ?port.id,
                    remote_id = ?remote_node.id,
                    error = &err as &dyn std::error::Error,
                    "port failed due to failed node"
                );
            }
        }
        pending_events.process();

        // Finally, forget the node.
        self.state.lock().nodes.remove(&remote_node.id);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::message::MeshField;
    use crate::resource::SerializedMessage;
    use futures::stream::Stream;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use pal_async::task::Task;
    use std::future::Future;
    use std::future::poll_fn;
    use std::marker::PhantomData;
    use std::pin::Pin;
    use std::pin::pin;
    use std::task::Context;
    use std::task::Poll;
    use test_with_tracing::test;

    fn yield_once() -> YieldOnce {
        YieldOnce { yielded: false }
    }

    struct YieldOnce {
        yielded: bool,
    }

    impl Future for YieldOnce {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if !self.yielded {
                self.yielded = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            ().into()
        }
    }

    #[derive(Debug)]
    pub enum TryRecvError {
        Empty,
        Closed,
        Failed,
    }

    #[derive(Debug)]
    pub enum RecvError {
        Closed,
        Failed,
    }

    struct Channel<T = SerializedMessage, U = SerializedMessage> {
        port: PortWithHandler<Queue>,
        _phantom: PhantomData<(fn(T), fn() -> U)>,
    }

    #[derive(Default)]
    struct Queue {
        queue: VecDeque<OwnedMessage>,
        closed: bool,
        failed: bool,
        waker: Option<Waker>,
    }

    impl Queue {
        fn try_recv(&mut self) -> Result<OwnedMessage, TryRecvError> {
            if let Some(x) = self.queue.pop_front() {
                Ok(x)
            } else if self.closed {
                Err(TryRecvError::Closed)
            } else if self.failed {
                Err(TryRecvError::Failed)
            } else {
                Err(TryRecvError::Empty)
            }
        }

        fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<OwnedMessage, RecvError>> {
            let r = if let Some(x) = self.queue.pop_front() {
                Ok(x)
            } else if self.closed {
                Err(RecvError::Closed)
            } else if self.failed {
                Err(RecvError::Failed)
            } else {
                self.waker = Some(cx.waker().clone());
                return Poll::Pending;
            };
            Poll::Ready(r)
        }
    }

    impl HandlePortEvent for Queue {
        fn message(
            &mut self,
            control: &mut PortControl<'_, '_>,
            message: Message<'_>,
        ) -> Result<(), HandleMessageError> {
            self.queue.push_back(message.into_owned());
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

        fn fail(&mut self, control: &mut PortControl<'_, '_>, _err: NodeError) {
            self.failed = true;
            if let Some(waker) = self.waker.take() {
                control.wake(waker);
            }
        }

        fn drain(&mut self) -> Vec<OwnedMessage> {
            self.queue.drain(..).collect()
        }
    }

    impl<T: MeshField, U: MeshField> From<Port> for Channel<T, U> {
        fn from(port: Port) -> Self {
            Self {
                port: port.set_handler(Queue::default()),
                _phantom: PhantomData,
            }
        }
    }

    impl<T, U> From<Channel<T, U>> for Port {
        fn from(channel: Channel<T, U>) -> Self {
            channel.port.remove_handler().0
        }
    }

    impl<T: 'static + MeshField + Send, U: 'static + MeshField + Send> Channel<T, U> {
        fn new_pair() -> (Self, Channel<U, T>) {
            let (left, right) = Port::new_pair();
            (left.into(), right.into())
        }

        fn bridge(self, other: Channel<U, T>) {
            Port::from(self).bridge(other.into())
        }

        fn change_types<T2: MeshField, U2: MeshField>(self) -> Channel<T2, U2> {
            let Self { port, _phantom: _ } = self;
            Channel {
                port,
                _phantom: PhantomData,
            }
        }

        fn send(&self, t: T) {
            self.port.send(Message::new((t,)));
        }

        fn try_recv(&mut self) -> Result<U, TryRecvError> {
            self.port
                .with_handler(|queue| queue.try_recv())
                .map(|m| m.parse::<(U,)>().unwrap().0)
        }

        async fn recv(&mut self) -> Result<U, RecvError> {
            poll_fn(|cx| self.port.with_handler(|queue| queue.poll_recv(cx)))
                .await
                .map(|m| m.parse::<(U,)>().unwrap().0)
        }
    }

    struct RemoteLocalNode {
        _task: Task<()>,
        node: Arc<LocalNode>,
        send: async_channel::Sender<RemoteEvent>,
    }

    struct RemoteEvent {
        node_id: NodeId,
        data: Vec<u8>,
        resources: Vec<OsResource>,
    }

    #[derive(Debug)]
    struct NullConnect;

    impl Connect for NullConnect {
        fn connect(&self, _node_id: NodeId, _handle: RemoteNodeHandle) {}
    }

    impl RemoteLocalNode {
        fn new(driver: &impl Spawn) -> Self {
            let (send, recv) = async_channel::unbounded::<RemoteEvent>();
            let node = Arc::new(LocalNode::with_id(NodeId::new(), Box::new(NullConnect)));
            let task = driver.spawn("test", {
                let node = node.clone();
                async move {
                    let mut recv = pin!(recv);
                    while let Some(mut event) = poll_fn(|cx| recv.as_mut().poll_next(cx)).await {
                        node.event(&event.node_id, &event.data, &mut event.resources);
                    }
                }
            });
            Self {
                _task: task,
                node,
                send,
            }
        }

        fn connect(self: &Arc<Self>, other: &Arc<Self>) -> RemoteNodeHandle {
            let handle = self.node.add_remote(other.node.id());
            handle.connect(EventsFrom {
                node_id: self.node.id(),
                send: other.send.clone(),
            });
            handle
        }
    }

    struct EventsFrom {
        node_id: NodeId,
        send: async_channel::Sender<RemoteEvent>,
    }

    impl SendEvent for EventsFrom {
        fn event(&self, event: OutgoingEvent<'_>) {
            let mut buffer = Vec::with_capacity(event.len());
            let mut os_resources = Vec::new();
            event.write_to(&mut buffer, &mut os_resources);
            self.send
                .send_blocking(RemoteEvent {
                    node_id: self.node_id,
                    data: buffer,
                    resources: os_resources,
                })
                .ok();
        }
    }

    #[test]
    fn test_local() {
        let (left, mut right) = Channel::<_, ()>::new_pair();
        left.send(SerializedMessage {
            data: b"abc".to_vec(),
            ..Default::default()
        });
        assert_eq!(right.try_recv().unwrap().data, b"abc");
        assert!(matches!(right.try_recv().unwrap_err(), TryRecvError::Empty));
    }

    fn new_two_node_mesh(
        driver: &DefaultDriver,
    ) -> (
        Arc<RemoteLocalNode>,
        Arc<RemoteLocalNode>,
        Vec<RemoteNodeHandle>,
    ) {
        let node = Arc::new(RemoteLocalNode::new(driver));
        let node2 = Arc::new(RemoteLocalNode::new(driver));
        let mut v = Vec::new();
        let handle = node.connect(&node2);
        v.push(handle);
        let handle = node2.connect(&node);
        v.push(handle);
        (node, node2, v)
    }

    fn new_three_node_mesh(
        driver: &DefaultDriver,
    ) -> (
        Arc<RemoteLocalNode>,
        Arc<RemoteLocalNode>,
        Arc<RemoteLocalNode>,
        Vec<RemoteNodeHandle>,
    ) {
        let node = Arc::new(RemoteLocalNode::new(driver));
        let node2 = Arc::new(RemoteLocalNode::new(driver));
        let node3 = Arc::new(RemoteLocalNode::new(driver));
        let mut v = Vec::new();
        for i in [&node, &node2, &node3][..].iter().copied() {
            for j in [&node, &node2, &node3][..].iter().copied() {
                if Arc::as_ptr(i) != Arc::as_ptr(j) {
                    let handle = i.connect(j);
                    v.push(handle);
                }
            }
        }
        (node, node2, node3, v)
    }

    fn new_remote_port_pair(node1: &LocalNode, node2: &LocalNode) -> (Channel, Channel) {
        let left_id = PortId::new();
        let right_id = PortId::new();
        let left = node1.add_port(
            left_id,
            Address {
                node: node2.id(),
                port: right_id,
            },
        );
        let right = node2.add_port(
            right_id,
            Address {
                node: node1.id(),
                port: left_id,
            },
        );
        (left.into(), right.into())
    }

    fn bmsg(data: &[u8]) -> SerializedMessage {
        SerializedMessage {
            data: data.into(),
            ..Default::default()
        }
    }

    #[async_test]
    async fn test_remote(driver: DefaultDriver) {
        let (node, node2, _h) = new_two_node_mesh(&driver);
        {
            let (left, mut right) = new_remote_port_pair(&node.node, &node2.node);
            left.send(SerializedMessage {
                data: b"abc".to_vec(),
                ..Default::default()
            });
            assert_eq!(right.recv().await.unwrap().data, b"abc");
        }
        yield_once().await;
        assert!(node.node.is_empty());
        assert!(node2.node.is_empty());
    }

    #[async_test]
    async fn test_send_port(driver: DefaultDriver) {
        let (node, node2, _h) = new_two_node_mesh(&driver);
        {
            let (left, mut right) = new_remote_port_pair(&node.node, &node2.node);
            let (left2, right2) = <Channel>::new_pair();
            left2.send(SerializedMessage {
                data: b"abc".to_vec(),
                ..Default::default()
            });
            left.send(SerializedMessage {
                resources: vec![Resource::Port(right2.into())],
                ..Default::default()
            });
            let r = right.recv().await.unwrap();
            let mut right2 =
                <Channel>::from(Port::try_from(r.resources.into_iter().next().unwrap()).unwrap());
            left2.send(SerializedMessage {
                data: b"def".to_vec(),
                ..Default::default()
            });
            assert_eq!(right2.recv().await.unwrap().data, b"abc");
            assert_eq!(right2.recv().await.unwrap().data, b"def");
        }
        yield_once().await;
        assert!(node.node.is_empty());
        assert!(node2.node.is_empty());
    }

    #[async_test]
    async fn test_send_port_with_three_nodes(driver: DefaultDriver) {
        let (node, node2, node3, _h) = new_three_node_mesh(&driver);
        {
            let (left, mut right) = new_remote_port_pair(&node.node, &node2.node);
            let (left2, right2) = new_remote_port_pair(&node3.node, &node.node);
            left2.send(SerializedMessage {
                data: b"abc".to_vec(),
                ..Default::default()
            });
            left.send(SerializedMessage {
                resources: vec![Resource::Port(right2.into())],
                ..Default::default()
            });
            let r = right.recv().await.unwrap();
            let mut right2 =
                <Channel>::from(Port::try_from(r.resources.into_iter().next().unwrap()).unwrap());
            left2.send(SerializedMessage {
                data: b"def".to_vec(),
                ..Default::default()
            });
            assert_eq!(right2.recv().await.unwrap().data, b"abc");
            assert_eq!(right2.recv().await.unwrap().data, b"def");
        }
        yield_once().await;
        assert!(node.node.is_empty());
        assert!(node2.node.is_empty());
        assert!(node3.node.is_empty());
    }

    #[async_test]
    async fn test_send_closed_port(driver: DefaultDriver) {
        let (node, node2, _h) = new_two_node_mesh(&driver);
        {
            let (left, mut right) = new_remote_port_pair(&node.node, &node2.node);
            let (left2, right2) = <Channel>::new_pair();
            drop(left2);
            left.send(SerializedMessage {
                resources: vec![Resource::Port(right2.into())],
                ..Default::default()
            });
            let r = right.recv().await.unwrap();
            let mut right2 =
                <Channel>::from(Port::try_from(r.resources.into_iter().next().unwrap()).unwrap());
            assert!(matches!(
                right2.try_recv().unwrap_err(),
                TryRecvError::Closed
            ));
        }
        yield_once().await;
        assert!(node.node.is_empty());
        assert!(node2.node.is_empty());
    }

    #[test]
    fn test_local_close() {
        let (left, mut right) = Channel::<_, ()>::new_pair();
        left.send(SerializedMessage {
            data: b"abc".to_vec(),
            ..Default::default()
        });
        drop(left);
        assert_eq!(right.try_recv().unwrap().data, b"abc");
        assert!(matches!(
            right.try_recv().unwrap_err(),
            TryRecvError::Closed
        ));
    }

    #[async_test]
    async fn test_remote_close(driver: DefaultDriver) {
        let (node, node2, _h) = new_two_node_mesh(&driver);
        {
            let (left, mut right) = new_remote_port_pair(&node.node, &node2.node);
            left.send(SerializedMessage {
                data: b"abc".to_vec(),
                ..Default::default()
            });
            drop(left);
            assert_eq!(right.recv().await.unwrap().data, b"abc");
            assert!(matches!(
                right.try_recv().unwrap_err(),
                TryRecvError::Closed
            ));
        }
        yield_once().await;
        assert!(node.node.is_empty());
        assert!(node2.node.is_empty());
    }

    #[async_test]
    async fn test_node_fail(driver: DefaultDriver) {
        let (node, node2, mut handles) = new_two_node_mesh(&driver);
        let (_left, mut right) = new_remote_port_pair(&node.node, &node2.node);
        handles.remove(1);
        assert!(matches!(
            right.try_recv().unwrap_err(),
            TryRecvError::Failed
        ));
    }

    #[async_test]
    async fn test_send_failed_port(driver: DefaultDriver) {
        let (node, node2, node3, mut handles) = new_three_node_mesh(&driver);
        let (_left, right) = new_remote_port_pair(&node.node, &node2.node);
        let (left2, mut right2) = new_remote_port_pair(&node2.node, &node3.node);
        handles.remove(2);
        left2.send(SerializedMessage {
            resources: vec![Resource::Port(right.into())],
            ..Default::default()
        });
        let r = right2.recv().await.unwrap();
        let mut right =
            <Channel>::from(Port::try_from(r.resources.into_iter().next().unwrap()).unwrap());
        assert!(matches!(
            right.try_recv().unwrap_err(),
            TryRecvError::Failed
        ));
    }

    #[async_test]
    async fn test_async(driver: DefaultDriver) {
        let (node, node2, _h) = new_two_node_mesh(&driver);
        let (left, mut right) = new_remote_port_pair(&node.node, &node2.node);
        let left = Arc::new(left);
        driver
            .spawn("test", {
                let left = left.clone();
                async move {
                    left.send(SerializedMessage {
                        data: b"abc".to_vec(),
                        ..Default::default()
                    });
                }
            })
            .detach();
        assert_eq!(right.recv().await.unwrap().data, b"abc");
        drop(left);
    }

    #[async_test]
    async fn test_async_close(driver: DefaultDriver) {
        let (node, node2, _h) = new_two_node_mesh(&driver);
        let (left, mut right) = new_remote_port_pair(&node.node, &node2.node);
        driver
            .spawn("test", async move {
                drop(left);
            })
            .detach();
        assert!(matches!(right.recv().await.unwrap_err(), RecvError::Closed));
    }

    #[async_test]
    async fn test_bridge_local() {
        let (p1, p2) = Channel::new_pair();
        let (p3, p4) = Channel::new_pair();
        test_bridge(p1, p2, p3, p4).await;
    }

    #[async_test]
    async fn test_bridge_remote(driver: DefaultDriver) {
        let (node, node2, node3, _h) = new_three_node_mesh(&driver);
        let (p1, p2) = new_remote_port_pair(&node.node, &node2.node);
        let (p3, p4) = new_remote_port_pair(&node2.node, &node3.node);
        test_bridge(p1, p2, p3, p4).await;
        node.node.wait_for_ports(true).await;
        node2.node.wait_for_ports(true).await;
        node3.node.wait_for_ports(true).await;
    }

    async fn test_bridge(p1: Channel, p2: Channel, mut p3: Channel, p4: Channel) {
        p1.send(bmsg(b"5"));
        p1.send(bmsg(b"6"));
        p1.send(bmsg(b"7"));

        p2.send(bmsg(b"a"));
        p2.send(bmsg(b"b"));

        p3.send(bmsg(b"1"));
        p3.send(bmsg(b"2"));
        p3.send(bmsg(b"3"));
        p3.send(bmsg(b"4"));

        p4.send(bmsg(b"x"));
        p4.send(bmsg(b"y"));
        p4.send(bmsg(b"c"));
        p4.send(bmsg(b"d"));
        p4.send(bmsg(b"e"));
        p4.send(bmsg(b"f"));
        p4.send(bmsg(b"g"));
        p4.send(bmsg(b"h"));

        p3.recv().await.unwrap();
        p3.recv().await.unwrap();

        p2.bridge(p3);

        p4.send(bmsg(b"i"));
        drop(p4);

        let recv_all = async |mut p: Channel| {
            let mut v = Vec::new();
            loop {
                match p.recv().await {
                    Ok(m) => v.push(m.data[0]),
                    Err(RecvError::Closed) => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(v)
        };

        assert_eq!(recv_all(p1).await.unwrap(), b"abcdefghi");
    }

    #[test]
    fn test_bridge_self() {
        let (p1, p2) = Channel::<(), ()>::new_pair();
        // This should fail the ports.
        p1.bridge(p2);
    }

    #[async_test]
    async fn test_fail_sent_port_to_failed_node(driver: DefaultDriver) {
        let (n1, n2, mut h) = new_two_node_mesh(&driver);
        let (p1, _p2) = new_remote_port_pair(&n1.node, &n2.node);
        let (mut p3, p4) = <Channel>::new_pair();
        p1.send(SerializedMessage {
            resources: vec![Resource::Port(p4.into())],
            ..Default::default()
        });
        h.remove(0);
        assert!(matches!(p3.recv().await.unwrap_err(), RecvError::Failed));
    }

    #[async_test]
    async fn test_close_drop_port_with_queued_ports() {
        let (p1, p2) = Channel::<_, ()>::new_pair();
        let (mut p3, p4) = <Channel>::new_pair();
        p1.send(SerializedMessage {
            resources: vec![Resource::Port(p4.into())],
            ..Default::default()
        });
        drop(p2);
        assert!(matches!(p3.recv().await.unwrap_err(), RecvError::Closed));
    }

    #[async_test]
    async fn test_close_send_port_to_dropped_port() {
        let (p1, p2) = Channel::<_, ()>::new_pair();
        let (mut p3, p4) = <Channel>::new_pair();
        drop(p2);
        p1.send(SerializedMessage {
            resources: vec![Resource::Port(p4.into())],
            ..Default::default()
        });
        assert!(matches!(p3.recv().await.unwrap_err(), RecvError::Closed));
    }

    #[async_test]
    async fn test_change_sender_types() {
        let (p1, mut p2) = Channel::<u32, ()>::new_pair();
        let p1 = p1.change_types::<u64, ()>();
        p1.send(1);
        assert_eq!(p2.recv().await.unwrap(), 1);
    }

    #[async_test]
    async fn test_change_receiver_types() {
        let (p1, p2) = Channel::<u32, ()>::new_pair();
        let mut p2 = p2.change_types::<(), u64>();
        p1.send(1);
        assert_eq!(p2.recv().await.unwrap(), 1);
    }

    #[async_test]
    async fn test_change_both_types() {
        let (p1, p2) = Channel::<u32, ()>::new_pair();
        let p1 = p1.change_types::<u64, ()>();
        let mut p2 = p2.change_types::<(), u64>();
        p1.send(1);
        assert_eq!(p2.recv().await.unwrap(), 1);
    }

    #[async_test]
    async fn test_change_from_generic() {
        let (p1, p2) = Channel::<SerializedMessage, SerializedMessage>::new_pair();
        let p1 = p1.change_types::<u64, ()>();
        let mut p2 = p2.change_types::<(), u32>();
        p1.send(1);
        assert_eq!(p2.recv().await.unwrap(), 1);
    }

    #[async_test]
    async fn test_fail_port(driver: DefaultDriver) {
        #[derive(Debug, Error)]
        #[error("test failure")]
        struct ExplicitFailure;

        let (node, node2, _h) = new_two_node_mesh(&driver);
        let (p1, mut p2) = new_remote_port_pair(&node.node, &node2.node);
        let p1 = Port::from(p1);
        p1.fail(NodeError::local(ExplicitFailure));
        let err = p2.recv().await.unwrap_err();
        assert!(matches!(err, RecvError::Failed));
    }
}
