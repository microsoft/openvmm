// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows ALPC-based mesh node implementation.

#![cfg(windows)]
// UNSAFETY: Needed to implement the Buffer trait.
#![expect(unsafe_code)]

use crate::common::InvitationAddress;
use crate::protocol;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::AbortHandle;
use futures::future::abortable;
use mesh_node::common::Address;
use mesh_node::common::NodeId;
use mesh_node::common::PortId;
use mesh_node::common::Uuid;
use mesh_node::local_node::Connect;
use mesh_node::local_node::LocalNode;
use mesh_node::local_node::OutgoingEvent;
use mesh_node::local_node::Port;
use mesh_node::local_node::RemoteNodeHandle;
use mesh_node::local_node::SendEvent;
use mesh_node::resource::OsResource;
use mesh_node::resource::Resource;
use mesh_protobuf::Protobuf;
use mesh_protobuf::buffer::Buffer;
use ntapi::ntobapi::DIRECTORY_ALL_ACCESS;
use pal::windows::BorrowedHandleExt;
use pal::windows::ObjectAttributes;
use pal::windows::OwnedSocketExt;
use pal::windows::UnicodeString;
use pal::windows::alpc;
use pal::windows::alpc::PortSection;
use pal::windows::alpc::SendMessage;
use pal::windows::create_object_directory;
use pal_async::driver::Driver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::wait::PolledWait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::io;
use std::ops::Deref;
use std::os::windows::prelude::*;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use tracing_helpers::ErrorValueExt;
use unicycle::FuturesUnordered;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

type InvitationMap =
    Arc<Mutex<HashMap<NodeId, (RemoteNodeHandle, mesh_channel::OneshotSender<()>)>>>;

/// The maximum ALPC message size to use. Messages larger than this will be
/// transferred in secure views.
///
/// This value was chosen arbitrarily and has not been performance tested.
const MAX_MESSAGE_SIZE: usize = 0x1000;
const MAX_SMALL_EVENT_SIZE: usize = MAX_MESSAGE_SIZE - size_of::<protocol::PacketHeader>();

/// A node within a mesh that uses Windows ALPC to communicate.
///
/// Each node within the mesh has an ALPC server port for incoming data and
/// multiple ALPC client ports for sending data to other nodes. ALPC port
/// communication is half duplex to simplify connection establishment and
/// lifetime management.
///
/// Since ALPC ports cannot be sent cross-process, the server ports are named
/// and exist within the Ob namespace. To prevent processes from outside the
/// mesh connecting to one of the ALPC ports, the server ports reside in an
/// anonymous Ob directory whose handle is duplicated into processes within the
/// mesh.
pub struct AlpcNode {
    driver: Box<dyn InvitationDriver>,
    local_node: Arc<LocalNode>,
    directory: Arc<OwnedHandle>,
    invitations: InvitationMap,
    recv_abort: AbortHandle,
    recv_task: Task<()>,
    connect_task: Task<()>,
    connect_send: async_channel::Sender<(NodeId, RemoteNodeHandle)>,
}

trait InvitationDriver: Spawn + Send {}

impl<T> InvitationDriver for T where T: Spawn + Send {}

/// A handle for an invitation, created by [`AlpcNode::invite`]. It can be
/// awaited to wait for the invitation to be accepted. When dropped, it will
/// cancel the invitation if it has not already been accepted.
#[derive(Debug)]
pub struct InvitationHandle {
    invitations: InvitationMap,
    local_id: NodeId,
    remote_id: NodeId,
    invitation_done: mesh_channel::OneshotReceiver<()>,
}

impl Future for InvitationHandle {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.invitation_done.poll_unpin(cx).map(|_| ())
    }
}

impl Drop for InvitationHandle {
    fn drop(&mut self) {
        let invitation = self.invitations.lock().remove(&self.remote_id);
        if let Some((handle, _invitation_done)) = invitation {
            tracing::warn!(
                node = ?self.local_id,
                remote_node = ?self.remote_id,
                "invitation dropped without connection",
            );
            // Invalidate the remote node.
            drop(handle);
        }
    }
}

/// An invitation allowing another process to join the mesh.
#[derive(Debug)]
pub struct Invitation {
    /// The common invitation addresses.
    pub address: InvitationAddress,
    /// The Ob directory that contains the mesh's ALPC server ports.
    pub directory: OwnedHandle,
}

#[derive(Debug, Protobuf)]
#[mesh(resource = "Resource")]
struct InitialMessage {
    user_port: Port,
}

/// The relative NT path to a server ALPC port within the anonymous directory.
fn node_path(node_id: NodeId) -> UnicodeString {
    format!("{:?}", &node_id).try_into().unwrap()
}

impl AlpcNode {
    /// Creates a node within a new mesh.
    pub fn new(driver: impl Driver + Spawn + Clone) -> io::Result<Self> {
        let directory = create_object_directory(&ObjectAttributes::new(), DIRECTORY_ALL_ACCESS)?;
        Self::with_id(driver, NodeId::new(), directory)
    }

    /// Gets the node ID. This is mostly useful for diagnostics.
    pub fn id(&self) -> NodeId {
        self.local_node.id()
    }

    /// Creates a node with the specified ID within an existing mesh.
    fn with_id(
        driver: impl Driver + Spawn + Clone,
        local_id: NodeId,
        directory: OwnedHandle,
    ) -> io::Result<Self> {
        let directory = Arc::new(directory);
        let port = alpc::PortConfig::new()
            .max_message_len(MAX_MESSAGE_SIZE)
            .waitable(true)
            .create(
                ObjectAttributes::new()
                    .root(directory.as_handle())
                    .name(&node_path(local_id)),
            )?;
        let port = PolledWait::new(&driver, port)?;

        let invitations = Default::default();
        let (connect_send, connect_recv) = async_channel::unbounded();
        let local_node = Arc::new(LocalNode::with_id(
            local_id,
            Box::new(AlpcConnector {
                connect_send: connect_send.clone(),
            }),
        ));

        // Start the connect task for handling connecting to remote nodes.
        let connect_task = driver.spawn("mesh alpc connect", {
            let directory = directory.clone();
            let driver = driver.clone();
            async move { Self::process_connects(&driver, local_id, directory, connect_recv).await }
        });

        // Start a receive task, which will be aborted in shutdown().
        let (fut, recv_abort) = abortable(Self::process_recv(
            local_id,
            local_node.clone(),
            port,
            Arc::clone(&invitations),
            connect_send.clone(),
        ));
        let recv_task = driver.spawn("mesh alpc recv", fut.map(drop));

        Ok(Self {
            driver: Box::new(driver),
            local_node,
            directory,
            invitations,
            recv_abort,
            recv_task,
            connect_task,
            connect_send,
        })
    }

    /// Async function for processing outgoing connection requests from the
    /// local node.
    async fn process_connects(
        driver: &(impl ?Sized + Driver),
        local_id: NodeId,
        directory: Arc<OwnedHandle>,
        connect_recv: async_channel::Receiver<(NodeId, RemoteNodeHandle)>,
    ) {
        let teardowns: Mutex<HashMap<NodeId, mesh_channel::OneshotSender<()>>> = Default::default();
        let mut connect_tasks = FuturesUnordered::new();
        let mut recv = std::pin::pin!(connect_recv);
        loop {
            // Receive a new request or drive any in-progress connection tasks
            // forward.
            let (remote_id, handle) = futures::select! { // merge semantics
                msg = recv.next().fuse() => {
                    match msg {
                        Some(msg) => msg,
                        None => break,
                    }
                }
                _ = connect_tasks.next() => continue,
            };

            let (teardown_send, teardown_recv) = mesh_channel::oneshot();
            teardowns.lock().insert(remote_id, teardown_send);
            connect_tasks.push(Self::process_one_connect(
                driver,
                local_id,
                remote_id,
                handle,
                &directory,
                teardown_recv,
                &teardowns,
            ));
        }

        // Signal connections to tear down and wait for them to be aborted.
        teardowns.lock().clear();
        while connect_tasks.next().await.is_some() {}
    }

    /// Makes a connection to the ALPC port for `remote_id`, returning the port
    /// after it has successfully connected.
    async fn connect_alpc(
        driver: &(impl ?Sized + Driver),
        directory: &OwnedHandle,
        local_id: NodeId,
        remote_id: NodeId,
    ) -> io::Result<PolledWait<AlpcPort>> {
        let data = (local_id.0).0;
        let port = alpc::PortConfig::new()
            .max_message_len(MAX_MESSAGE_SIZE)
            .waitable(true)
            .connect(
                ObjectAttributes::new()
                    .root(directory.as_handle())
                    .name(&node_path(remote_id)),
                &data,
            )?;

        let mut port = PolledWait::new(driver, AlpcPort(Arc::new(port)))?;
        port.wait().await?;
        let mut message = alpc::RecvMessageBuffer::new(0);
        let message = port
            .get()
            .try_recv(&mut message)?
            .expect("message should be available");
        match message.message_type() {
            alpc::MessageType::ConnectionReply | alpc::MessageType::Canceled => {
                // Canceled would indicate that the server has gone away after
                // accepting the offer. Since this is a race, act as though the
                // offer were successful--it will be failed soon.
            }
            ty => panic!("unexpected type {:?}", ty),
        }
        if message.needs_reply() {
            port.get().reply(message, &[])?;
        } else {
            drop(message);
        }
        Ok(port)
    }

    async fn process_one_connect(
        driver: &(impl ?Sized + Driver),
        local_id: NodeId,
        remote_id: NodeId,
        handle: RemoteNodeHandle,
        directory: &OwnedHandle,
        teardown_recv: mesh_channel::OneshotReceiver<()>,
        teardowns: &Mutex<HashMap<NodeId, mesh_channel::OneshotSender<()>>>,
    ) {
        tracing::debug!(node = ?local_id, remote_node = ?remote_id, "connecting to node");
        match Self::connect_alpc(driver, directory, local_id, remote_id).await {
            Ok(mut port) => {
                let (failed_send, failed_recv) = mesh_channel::oneshot();
                handle.connect(Connection {
                    local_id,
                    remote_id,
                    port: port.get().clone(),
                    failed_send: Mutex::new(Some(failed_send)),
                });
                tracing::debug!(node = ?local_id, remote_node = ?remote_id, "connected to node");

                // Wait for the ALPC port to be closed by the other endpoint, or
                // for a teardown request, or for a send to fail.
                //
                // N.B. when shutdown() is called, this await will be aborted.
                let result = futures::select! { // race semantics
                    _ = port.wait().fuse() => {
                        tracing::debug!(node = ?local_id, remote_node = ?remote_id, "remote node disconnection");
                        Ok(false)
                    }
                    r = failed_recv.fuse() => {
                        if let Ok(err) = r {
                            tracing::error!(
                                node = ?local_id,
                                remote_node = ?remote_id,
                                error = &err as &dyn std::error::Error,
                                "disconnecting from node due to failure");

                            Err(err)
                        } else {
                            // The node was disconnected in the receive path.
                            Ok(false)
                        }
                    }
                    _ = teardown_recv.fuse() => Ok(true),
                };
                match result {
                    Ok(true) => {
                        tracing::trace!(
                            node = ?local_id,
                            remote_node = ?remote_id,
                            "tearing down, sending flush request",
                        );
                        match port.get().request(&[]) {
                            Ok(_) => port.wait().await.expect("wait on handle cannot fail"),
                            Err(error) => {
                                // ERROR_INVALID_HANDLE is returned if the server
                                // disconnected the node before the flush, which is
                                // expected to occur sometimes depending on the
                                // order of teardown.
                                if error.raw_os_error() != Some(6 /* ERROR_INVALID_HANDLE */) {
                                    tracing::error!(
                                        node = ?local_id,
                                        remote_node = ?remote_id,
                                        error = error.as_error(),
                                        "failed to send node flush",
                                    );
                                }
                            }
                        }
                        tracing::debug!(node = ?local_id, remote_node = ?remote_id, "disconnected from remote node");
                        handle.disconnect();
                    }
                    Ok(false) => {
                        handle.disconnect();
                    }
                    Err(err) => {
                        handle.fail(err);
                    }
                }
                teardowns.lock().remove(&remote_id);
            }
            Err(err) => {
                tracing::error!(
                    node = ?local_id,
                    remote_node = ?remote_id,
                    error = err.as_error(),
                    "node connection failed"
                );
                handle.fail(err);
            }
        }
    }

    /// Invites a new node to join the mesh, returning information to be passed
    /// to the new process and to be passed to [`AlpcNode::join`]. Bridges
    /// `port` with the initial port.
    pub fn invite(&self, port: Port) -> io::Result<(Invitation, InvitationHandle)> {
        // Get an inheritable handle for the invitation.
        let directory = self.directory.as_handle().duplicate(true, Some(0))?;

        let local_addr = Address {
            node: self.local_node.id(),
            port: PortId::new(),
        };
        let remote_addr = Address {
            node: NodeId::new(),
            port: PortId::new(),
        };
        let (invitation_done_send, invitation_done_recv) = mesh_channel::oneshot();
        let handle = self.local_node.add_remote(remote_addr.node);
        self.invitations
            .lock()
            .insert(remote_addr.node, (handle, invitation_done_send));

        let mut init_recv = <mesh_channel::OneshotReceiver<InitialMessage>>::from(
            self.local_node.add_port(local_addr.port, remote_addr),
        );

        // Wait for a message from the invitee before sending anything so that
        // we don't send any events to the new node until it's ready.
        self.driver
            .spawn("mesh alpc invitation", async move {
                match (&mut init_recv).await {
                    Ok(init_message) => {
                        tracing::trace!(
                            node = ?local_addr.node,
                            remote_node = ?remote_addr.node,
                            "received initial message",
                        );
                        init_message.user_port.bridge(port);
                    }
                    Err(err) => {
                        tracing::error!(
                            node = ?local_addr.node,
                            remote_node = ?remote_addr.node,
                            error = err.as_error(),
                            "invitation initial message failed",
                        );
                        // The port is closed or has failed. Bridge the port
                        // with the user port to reflect the failure back to the
                        // caller.
                        Port::from(init_recv).bridge(port);
                    }
                }
            })
            .detach();

        Ok((
            Invitation {
                address: InvitationAddress {
                    local_addr: remote_addr,
                    remote_addr: local_addr,
                },
                directory,
            },
            InvitationHandle {
                local_id: local_addr.node,
                remote_id: remote_addr.node,
                invitations: self.invitations.clone(),
                invitation_done: invitation_done_recv,
            },
        ))
    }

    /// Joins the ALPC mesh using the invitation from [`AlpcNode::invite`],
    /// bridging `port` with the initial port.
    pub fn join(
        driver: impl Driver + Spawn + Clone,
        invitation: Invitation,
        port: Port,
    ) -> io::Result<Self> {
        let node = Self::with_id(
            driver,
            invitation.address.local_addr.node,
            invitation.directory,
        )?;
        let init_port =
            mesh_channel::OneshotSender::<InitialMessage>::from(node.local_node.add_port(
                invitation.address.local_addr.port,
                invitation.address.remote_addr,
            ));

        // Notify the inviter that this node is ready by sending the initial port.
        init_port.send(InitialMessage { user_port: port });
        Ok(node)
    }

    /// Shuts down the node, waiting for any sent messages to be sent to their
    /// destination.
    ///
    /// After this call, any active ports will no longer be able to receive
    /// messages.
    ///
    /// It is essential to call this before exiting a mesh process; until this
    /// returns, data loss could occur for other mesh nodes.
    pub async fn shutdown(self) {
        self.local_node.wait_for_ports(false).await;
        self.connect_send.close();
        self.connect_task.await;
        self.recv_abort.abort();
        self.recv_task.await;
    }

    /// Asynchronously processes incoming ALPC messages from remote nodes.
    async fn process_recv(
        local_id: NodeId,
        local_node: Arc<LocalNode>,
        mut port: PolledWait<alpc::Port>,
        invitations: InvitationMap,
        connect_send: async_channel::Sender<(NodeId, RemoteNodeHandle)>,
    ) -> io::Result<()> {
        struct Connection {
            comm: alpc::Port,
            handle: RemoteNodeHandle,
        }

        let mut message = alpc::RecvMessageBuffer::new(MAX_MESSAGE_SIZE);
        let mut connections = HashMap::<_, Connection>::new();
        let mut next_id: usize = 1;
        let mut handles = Vec::new();
        let mut resources = Vec::new();
        loop {
            port.wait().await?;
            let mut message = port
                .get()
                .try_recv(&mut message)
                .inspect_err(|error| {
                    tracing::error!(
                        node = ?local_id,
                        error = error.as_error(),
                        "alpc error"
                    );
                })?
                .expect("port should have a message");
            let buf = message.data();
            match message.message_type() {
                alpc::MessageType::Datagram => {
                    message
                        .handles(port.get(), &mut handles)
                        .inspect_err(|error| {
                            tracing::error!(
                                node = ?local_id,
                                error = error.as_error(),
                                "alpc error getting handles"
                            );
                        })?;
                    // It's not easy to distinguish a socket from other file
                    // handles, so don't attempt to. `Resource::socket()`
                    // will convert from a handle as necessary.
                    resources.extend(handles.drain(..).map(OsResource::Handle));

                    let connection = connections
                        .get(&message.context())
                        .expect("port must exist");

                    match protocol::PacketHeader::read_from_prefix(buf) {
                        Ok((header, _)) => match header.packet_type {
                            // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                            protocol::PacketType::EVENT => {
                                local_node.event(
                                    connection.handle.id(),
                                    &buf[size_of_val(&header)..],
                                    &mut resources,
                                );
                            }
                            protocol::PacketType::LARGE_EVENT => {
                                if let Some(view) = message.secure_view() {
                                    local_node.event(connection.handle.id(), &view, &mut resources);
                                } else {
                                    tracing::error!(node = ?local_id, "missing secure view");
                                }
                            }
                            packet_type => {
                                tracing::error!(node = ?local_id, ?packet_type, "unknown packet type");
                            }
                        },
                        Err(_) => {
                            // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
                            tracing::error!(node = ?local_id, "invalid message");
                        }
                    }
                    resources.clear();
                    if message.needs_reply() {
                        port.get().reply(message, &[])?;
                    }
                }
                alpc::MessageType::ConnectionRequest => {
                    if let Ok(node_id) = buf.try_into().map(|uuid| NodeId(Uuid(uuid))) {
                        let handle = if let Some((handle, _invitation_done)) =
                            invitations.lock().remove(&node_id)
                        {
                            // If this is a connection from a node that was invited,
                            // then it's now safe to connect to the node. Send the
                            // deferred connection to the connection task.
                            let _ = connect_send.send_blocking((node_id, handle.clone()));
                            handle
                        } else {
                            // Otherwise, establish a connection now to get a
                            // handle in case something goes wrong.
                            local_node.get_remote_handle(node_id)
                        };

                        // Accept the ALPC connection.
                        match port.get().accept(
                            alpc::PortConfig::new().max_message_len(MAX_MESSAGE_SIZE),
                            message,
                            next_id,
                            &mut SendMessage::new(),
                        ) {
                            Ok(comm) => {
                                tracing::trace!(node = ?local_id, remote_node = ?node_id, "accepted connection");
                                connections.insert(next_id, Connection { comm, handle });
                                next_id += 1;
                            }
                            Err(err) => {
                                tracing::error!(
                                    node = ?local_id,
                                    remote_node = ?node_id,
                                    error = err.as_error(),
                                    "failed to accept connection"
                                );
                                handle.fail(err);
                            }
                        };
                    } else {
                        tracing::error!(node = ?local_id, "invalid connection request");
                        let _ = port.get().reject(message);
                    }
                }
                alpc::MessageType::PortClosed => {
                    assert!(!message.needs_reply());
                    let connection = connections
                        .remove(&message.context())
                        .expect("port must exist");

                    connection.handle.disconnect();
                }
                alpc::MessageType::ConnectionReply | alpc::MessageType::Reply => unreachable!(),
                alpc::MessageType::Canceled => {
                    assert!(!message.needs_reply());
                }
                alpc::MessageType::Request => {
                    // This is a flush request to make sure all datagram
                    // messages have been received.
                    let connection = connections
                        .get(&message.context())
                        .expect("port must exist");
                    let _ = connection.comm.reply(message, &[]);
                }
            }
        }
    }
}

/// Connector for connecting to remote ALPC nodes.
#[derive(Debug)]
struct AlpcConnector {
    connect_send: async_channel::Sender<(NodeId, RemoteNodeHandle)>,
}

impl Connect for AlpcConnector {
    fn connect(&self, remote_id: NodeId, handle: RemoteNodeHandle) {
        // Send the handle to the connect task.
        let _ = self.connect_send.send_blocking((remote_id, handle));
    }
}

/// An ALPC connection to a remote node's server port.
#[derive(Debug)]
struct Connection {
    local_id: NodeId,
    remote_id: NodeId,
    port: AlpcPort,
    failed_send: Mutex<Option<mesh_channel::OneshotSender<io::Error>>>,
}

struct AlpcMessageBuffer<'a>(&'a mut SendMessage);

impl Buffer for AlpcMessageBuffer<'_> {
    unsafe fn unwritten(&mut self) -> &mut [std::mem::MaybeUninit<u8>] {
        self.0.spare_capacity_mut()
    }

    unsafe fn extend_written(&mut self, len: usize) {
        // SAFETY: guaranteed by caller.
        unsafe {
            self.0.set_len(self.0.len() + len);
        }
    }
}

impl Connection {
    fn send_event(&self, event: OutgoingEvent<'_>) -> io::Result<()> {
        let mut message;
        let mut resources = Vec::new();
        let mut section_view = None;
        let section;
        if event.len() > MAX_SMALL_EVENT_SIZE {
            message = SendMessage::from(
                protocol::PacketHeader {
                    packet_type: protocol::PacketType::LARGE_EVENT,
                    ..FromZeros::new_zeroed()
                }
                .as_bytes(),
            );
            section = PortSection::new_secure(&self.port, event.len())?;
            let mut view = section.alloc_view(event.len())?;
            let mut b = io::Cursor::new(&mut *view);
            event.write_to(&mut b, &mut resources);
            section_view = Some(view);
        } else {
            let cap = size_of::<protocol::PacketHeader>() + event.len();
            message = SendMessage::with_capacity(cap);
            message.extend(
                protocol::PacketHeader {
                    packet_type: protocol::PacketType::EVENT,
                    ..FromZeros::new_zeroed()
                }
                .as_bytes(),
            );
            event.write_to(&mut AlpcMessageBuffer(&mut message), &mut resources);
        }

        let mut op = self.port.start_send(&mut message);
        for resource in resources.iter_mut() {
            match resource {
                OsResource::Handle(handle) => op.add_handle((*handle).as_handle()),
                OsResource::Socket(socket) => op.add_handle(socket.prepare_to_send()?),
            };
        }
        if let Some(view) = section_view {
            op.set_view(view.into_inner(), true);
        }
        op.send()?;
        Ok(())
    }
}

impl SendEvent for Connection {
    fn event(&self, event: OutgoingEvent<'_>) {
        let len = event.len();
        match self.send_event(event) {
            Ok(_) => (),
            Err(err) => {
                tracing::error!(
                    node = ?self.local_id,
                    remote_node = ?self.remote_id,
                    error = err.as_error(),
                    len,
                    "error sending packet"
                );
                // Notify the connection task of the failure.
                let send = self.failed_send.lock().take();
                if let Some(send) = send {
                    send.send(err);
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct AlpcPort(Arc<alpc::Port>);

impl AsHandle for AlpcPort {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl Deref for AlpcPort {
    type Target = alpc::Port;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::AlpcNode;
    use mesh_channel::RecvError;
    use mesh_channel::channel;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_event::Event;
    use std::io::Read;
    use std::io::Write;
    use test_with_tracing::test;
    use unix_socket::UnixStream;

    #[async_test]
    async fn test_two(driver: DefaultDriver) {
        let (send, recv) = channel::<u32>();
        let node1 = AlpcNode::new(driver.clone()).unwrap();
        let (invitation, _handle) = node1.invite(recv.into()).unwrap();

        let (send2, mut recv2) = channel::<u32>();
        let node2 = AlpcNode::join(driver, invitation, send2.into()).unwrap();

        send.send(5);
        assert_eq!(recv2.recv().await.unwrap(), 5);
        drop(send);
        drop(recv2);
        node1.shutdown().await;
        node2.shutdown().await;
    }

    #[async_test]
    async fn test_huge_message(driver: DefaultDriver) {
        let (send, recv) = channel::<Vec<u8>>();
        let node1 = AlpcNode::new(driver.clone()).unwrap();
        let (invitation, _handle) = node1.invite(recv.into()).unwrap();

        let (send2, mut recv2) = channel::<Vec<u8>>();
        let node2 = AlpcNode::join(driver, invitation, send2.into()).unwrap();

        let v = vec![0xcd; 8 << 20];
        send.send(v.clone());
        assert_eq!(recv2.recv().await.unwrap(), v);
        drop(send);
        drop(recv2);
        node1.shutdown().await;
        node2.shutdown().await;
    }

    #[async_test]
    async fn test_message_sizes(driver: DefaultDriver) {
        let (p1, p2) = mesh_node::local_node::Port::new_pair();
        let (p3, p4) = mesh_node::local_node::Port::new_pair();
        let node1 = AlpcNode::new(driver.clone()).unwrap();
        let (invitation, _handle) = node1.invite(p2).unwrap();
        let _node2 = AlpcNode::join(driver.clone(), invitation, p3).unwrap();

        crate::test_common::test_message_sizes(p1, p4, 0..=super::MAX_SMALL_EVENT_SIZE + 0x1000)
            .await;
    }

    #[async_test]
    async fn test_three(driver: DefaultDriver) {
        let (p1, p2) = channel::<u32>();
        let (p3, mut p4) = channel::<u32>();
        let (p5, p6) = channel::<u32>();
        let (p7, p8) = channel::<u32>();

        let node1 = AlpcNode::new(driver.clone()).unwrap();

        let (invitation, _handle) = node1.invite(p2.into()).unwrap();
        let node2 = AlpcNode::join(driver.clone(), invitation, p3.into()).unwrap();

        let (invitation, _handle) = node1.invite(p5.into()).unwrap();
        let node3 = AlpcNode::join(driver, invitation, p8.into()).unwrap();

        p1.bridge(p6);

        p7.send(5);

        assert_eq!(p4.recv().await.unwrap(), 5);
        drop(p4);
        drop(p7);
        node1.shutdown().await;
        node2.shutdown().await;
        node3.shutdown().await;
    }

    #[async_test]
    async fn test_handle(driver: DefaultDriver) {
        let (send, irecv) = channel::<Event>();
        let (isend, mut recv) = channel::<Event>();
        let node1 = AlpcNode::new(driver.clone()).unwrap();
        let (invitation, _handle) = node1.invite(irecv.into()).unwrap();
        let node2 = AlpcNode::join(driver, invitation, isend.into()).unwrap();
        let e = Event::new();
        send.send(e.clone());
        let e2 = recv.recv().await.unwrap();

        e.signal();
        e2.wait();
        drop(send);
        drop(recv);
        node1.shutdown().await;
        node2.shutdown().await;
    }

    #[async_test]
    async fn test_socket(driver: DefaultDriver) {
        let (send, irecv) = channel::<UnixStream>();
        let (isend, mut recv) = channel::<UnixStream>();
        let node1 = AlpcNode::new(driver.clone()).unwrap();
        let (invitation, _handle) = node1.invite(irecv.into()).unwrap();
        let _node2 = AlpcNode::join(driver, invitation, isend.into()).unwrap();
        let (mut s1, s2) = UnixStream::pair().unwrap();
        send.send(s2);
        let mut s2 = recv.recv().await.unwrap();

        s1.write_all(b"abc").unwrap();
        drop(s1);
        let mut buf = Vec::new();
        s2.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, b"abc");
    }

    #[async_test]
    async fn test_failed_invitation(driver: DefaultDriver) {
        let node = AlpcNode::new(driver).unwrap();
        let (send, mut recv) = channel::<()>();
        node.invite(send.into()).unwrap();
        assert!(matches!(
            recv.recv().await.unwrap_err(),
            RecvError::Error(_)
        ));
        drop(recv);
        node.shutdown().await;
    }

    #[async_test]
    async fn test_failed_node(driver: DefaultDriver) {
        let (_send, irecv) = channel::<()>();
        let (isend, _recv) = channel::<()>();
        let node1 = AlpcNode::new(driver.clone()).unwrap();
        let (invitation, _handle) = node1.invite(irecv.into()).unwrap();
        let node2 = AlpcNode::join(driver, invitation, isend.into()).unwrap();
        drop(node1);
        node2.shutdown().await;
    }

    #[async_test]
    async fn test_handoff_invitation(driver: DefaultDriver) {
        let (p1, p2) = channel::<mesh_channel::Sender<()>>();
        let (p3, mut p4) = channel::<mesh_channel::Sender<()>>();
        let (p5, p6) = channel::<()>();
        let (p7, mut p8) = channel::<()>();

        let node1 = AlpcNode::new(driver.clone()).unwrap();
        let (invitation, _handle) = node1.invite(p2.into()).unwrap();
        let node2 = AlpcNode::join(driver.clone(), invitation, p3.into()).unwrap();

        let (invitation, _handle) = node1.invite(p6.into()).unwrap();
        p1.send(p5);
        let p5p = p4.recv().await.unwrap();

        let node3 = AlpcNode::join(driver, invitation, p7.into()).unwrap();
        p5p.send(());
        p8.recv().await.unwrap();

        drop((p1, p4, p5p, p8));
        node1.shutdown().await;
        node2.shutdown().await;
        node3.shutdown().await;
    }
}
