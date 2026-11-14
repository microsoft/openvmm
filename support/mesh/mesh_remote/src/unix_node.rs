// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix socket-based mesh node implementation.
//!
//! Each pair of nodes communicate using a single, bidirectional Unix socket. On
//! platforms that support it (Linux), a `SOCK_SEQPACKET` socket is used, which
//! provides message framing and atomic message sends. Otherwise, a
//! `SOCK_STREAM` packet is used, and the protocol includes a message size.
//!
//! File descriptors are sent between nodes using the `SCM_RIGHTS` functionality
//! of Unix sockets. One message carries at most
//! [`protocol::MAX_FDS_PER_MESSAGE`] of them; an event with more sends the
//! rest ahead of it in `EVENT_FDS` packets.

#![cfg(unix)]

#[cfg(target_os = "linux")]
mod memfd;

use crate::common::InvitationAddress;
use crate::protocol;
use futures::FutureExt;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::future;
use futures::future::BoxFuture;
use io::ErrorKind;
use mesh_channel::OneshotReceiver;
use mesh_channel::OneshotSender;
use mesh_channel::RecvError;
use mesh_channel::channel;
use mesh_channel::oneshot;
use mesh_node::common::Address;
use mesh_node::common::NodeId;
use mesh_node::common::PortId;
use mesh_node::local_node::Connect;
use mesh_node::local_node::LocalNode;
use mesh_node::local_node::OutgoingEvent;
use mesh_node::local_node::Port;
use mesh_node::local_node::RemoteNodeHandle;
use mesh_node::local_node::SendEvent;
use mesh_node::resource::OsResource;
use mesh_node::resource::Resource;
use mesh_protobuf::Protobuf;
use pal_async::driver::SpawnDriver;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use socket2::Socket;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::future::Future;
use std::future::poll_fn;
use std::io;
use std::io::IoSlice;
use std::os::unix::prelude::*;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use thiserror::Error;
use tracing::instrument;
use unicycle::FuturesUnordered;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// If true, use a SOCK_SEQPACKET socket. Otherwise, use a SOCK_STREAM socket.
///
/// SOCK_SEQPACKET is preferred where available because it allows us to avoid
/// separately tracking message boundaries. Most importantly, this makes it
/// straightforward to support sending messages from multiple threads
/// simultaneously.
const USE_SEQPACKET: bool = cfg!(target_os = "linux");

/// The maximum packet size. Linux uses memfd to send data larger than this.
/// Other OSes just fail, so choose a larger size.
///
/// These values were chosen arbitrarily and have not been tested for
/// performance.
const MAX_PACKET_SIZE: usize = if cfg!(target_os = "linux") {
    0x4000
} else {
    0x40000
};

const MAX_SMALL_EVENT_SIZE: usize = MAX_PACKET_SIZE - size_of::<protocol::PacketHeader>();

/// A node within a mesh that uses Unix sockets to communicate.
///
/// Each pairwise connection between two nodes in the mesh communicates via a
/// pair of bidirectional sockets.
///
/// If one node needs to send data to another but does not have a connection, it
/// sends a request to the leader node to establish a connection. The leader
/// creates a new socket pair and sends one end to each of the nodes, which the
/// two nodes can use to communicate.
pub struct UnixNode {
    driver: Arc<dyn SpawnDriver>,
    local_node: Arc<LocalNode>,
    to_leader: Arc<mesh_channel::Sender<LeaderRequest>>,
    tasks: Arc<mesh_channel::Sender<SmallTask>>,
    io_task: Task<()>,
    // TODO: consider reducing type complexity?
    leader_resign_send:
        Mutex<Option<Arc<mesh_channel::Sender<(NodeId, mesh_channel::Sender<Followers>)>>>>,

    // meaningful drop
    _drop_send: OneshotSender<()>,
}

/// Error returned when creating a mesh invitation fails.
#[derive(Debug, Error)]
#[error("mesh node shut down before invitation could be created")]
pub struct InviteError;

/// A Clone + Send handle for creating mesh invitations.
///
/// Extracted from a [`UnixNode`] via [`UnixNode::inviter()`]. This delegates
/// invitation requests to the node's leader task via the existing
/// `LeaderRequest::Invite` channel.
///
/// This allows creating invitations from any async task without holding a
/// reference to the node.
#[derive(Clone)]
pub(crate) struct UnixMeshInviter {
    to_leader: mesh_channel::Sender<LeaderRequest>,
}

impl UnixMeshInviter {
    /// Create a mesh invitation for a new node to join.
    ///
    /// The returned [`Invitation`] can be sent to another process to allow
    /// it to join the mesh.
    pub(crate) async fn invite(&self, port: Port) -> Result<Invitation, InviteError> {
        let (invitation_send, mut invitation_recv) = channel();
        self.to_leader
            .send(LeaderRequest::Invite(port, invitation_send));
        invitation_recv.recv().await.map_err(|_| InviteError)
    }
}

#[derive(Debug, Protobuf)]
#[mesh(resource = "Resource")]
enum LeaderRequest {
    Connect(NodeId),
    Invite(Port, mesh_channel::Sender<Invitation>),
}

#[derive(Debug, Protobuf)]
#[mesh(resource = "Resource")]
enum FollowerRequest {
    Connect(
        NodeId,
        #[mesh(
            encoding = "mesh_protobuf::encoding::OptionField<mesh_protobuf::encoding::ResourceField<OwnedFd>>"
        )]
        Option<Socket>,
    ),
}

#[derive(Debug, Protobuf)]
#[mesh(resource = "Resource")]
pub struct Followers {
    list: Vec<(
        NodeId,
        mesh_channel::Receiver<LeaderRequest>,
        mesh_channel::Sender<FollowerRequest>,
    )>,
}

#[derive(Debug, Protobuf)]
#[mesh(resource = "Resource")]
struct InitialMessage {
    leader_send: mesh_channel::Sender<LeaderRequest>,
    follower_recv: mesh_channel::Receiver<FollowerRequest>,
    user_port: Port,
}

/// Processes incoming requests from the leader to a follower. Currently the only
/// such request is to add a connection to another node.
#[instrument(skip_all, fields(local_id = ?local_node.id()))]
async fn run_follower(
    driver: &dyn SpawnDriver,
    local_node: &Arc<LocalNode>,
    mut recv: mesh_channel::Receiver<FollowerRequest>,
    pending_connections: Arc<Mutex<HashMap<NodeId, RemoteNodeHandle>>>,
    tasks: &mesh_channel::Sender<SmallTask>,
) {
    while let Ok(req) = recv.recv().await {
        match req {
            FollowerRequest::Connect(target_id, fd) => {
                tracing::debug!(?target_id, "got connection request from leader");
                let handle = pending_connections.lock().remove(&target_id);
                let handle = handle.unwrap_or_else(|| local_node.get_remote_handle(target_id));

                if let Some(fd) = fd {
                    start_connection(
                        tasks,
                        local_node,
                        target_id,
                        handle,
                        UnixSocket::new(driver, fd),
                    );
                } else {
                    tracing::warn!(?target_id, "leader provided failed connection");
                }
            }
        }
    }
}

/// Processes incoming requests from a follower to the leader. Runs until there
/// are no more followers or until the leader is asked to transfer power to
/// another node via `resign_recv`.
#[instrument(skip_all, fields(local_id = ?local_node.id()))]
async fn run_leader(
    driver: &dyn SpawnDriver,
    local_node: &Arc<LocalNode>,
    mut resign_recv: mesh_channel::Receiver<(NodeId, mesh_channel::Sender<Followers>)>,
    followers: Followers,
    tasks: &mesh_channel::Sender<SmallTask>,
) {
    let mut senders = HashMap::new();
    let mut receivers = Vec::new();
    for (remote_id, recv, send) in followers.list {
        receivers.push((remote_id, recv));
        senders.insert(remote_id, send);
    }

    let new_leader_info = loop {
        if receivers.is_empty() {
            return;
        }
        let recvs = receivers
            .iter_mut()
            .map(|(_, recv)| poll_fn(|cx| recv.poll_recv(cx)));
        let (req, index, _) = futures::select! { // merge semantics
            r = resign_recv.next() => break r,
            r = future::select_all(recvs).fuse() => r,
        };
        let remote_id = receivers[index].0;
        match req {
            Ok(req) => match req {
                LeaderRequest::Connect(target_id) => {
                    tracing::debug!(?target_id, ?remote_id, "connection request");
                    let remote = senders
                        .get(&remote_id)
                        .expect("sender must exist to receive from it");
                    let mut fd = None;
                    if let Some(target) = senders.get(&target_id) {
                        match new_socket_pair() {
                            Ok((left, right)) => {
                                tracing::trace!(?target, "send to");
                                target.send(FollowerRequest::Connect(remote_id, Some(left)));
                                fd = Some(right);
                            }
                            Err(err) => {
                                tracing::warn!(
                                    ?target_id,
                                    ?remote_id,
                                    error = &err as &dyn std::error::Error,
                                    "failed to create socket pair for connection request"
                                );
                            }
                        }
                    } else {
                        tracing::warn!(?target_id, ?remote_id, "could not find target for remote");
                    }
                    remote.send(FollowerRequest::Connect(target_id, fd));
                }
                LeaderRequest::Invite(port, send) => {
                    tracing::debug!(?remote_id, "invitation request");
                    match new_socket_pair() {
                        Ok((left, right)) => {
                            let (leader_send, leader_recv) = channel();
                            let (follower_send, follower_recv) = channel();
                            let remote_addr = Address {
                                node: NodeId::new(),
                                port: PortId::new(),
                            };
                            let local_port_id = PortId::new();
                            let handle = local_node.add_remote(remote_addr.node);
                            start_connection(
                                tasks,
                                local_node,
                                remote_addr.node,
                                handle,
                                UnixSocket::new(driver, left),
                            );
                            let init_send = OneshotSender::<InitialMessage>::from(
                                local_node.add_port(local_port_id, remote_addr),
                            );
                            init_send.send(InitialMessage {
                                leader_send,
                                follower_recv,
                                user_port: port,
                            });
                            let invitation = Invitation {
                                address: InvitationAddress {
                                    local_addr: remote_addr,
                                    remote_addr: Address {
                                        node: local_node.id(),
                                        port: local_port_id,
                                    },
                                },
                                fd: right.into(),
                            };
                            tracing::debug!(
                                invite_id = ?invitation.address.local_addr.node,
                                ?remote_id,
                                "inviting",
                            );
                            send.send(invitation);
                            senders.insert(remote_addr.node, follower_send);
                            receivers.push((remote_addr.node, leader_recv));
                        }
                        Err(err) => {
                            tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "failed to create socket pair",
                            );
                        }
                    }
                }
            },
            Err(err) => {
                if let RecvError::Error(err) = err {
                    tracing::debug!(
                        ?remote_id,
                        error = &err as &dyn std::error::Error,
                        "leader connection to remote failed"
                    );
                }
                senders.remove(&remote_id);
                receivers.swap_remove(index);
            }
        }
    };

    if let Some((new_leader_id, new_leader_followers_sink)) = new_leader_info {
        if let Some(new_leader_send) = senders.get(&new_leader_id) {
            tracing::debug!(?new_leader_id, "resigning leadership");
            // Ensure there is a connection between every follower and the new
            // leader.
            for (remote_id, send) in senders.iter() {
                if new_leader_id != *remote_id {
                    match new_socket_pair() {
                        Ok((left, right)) => {
                            send.send(FollowerRequest::Connect(new_leader_id, Some(left)));
                            new_leader_send.send(FollowerRequest::Connect(*remote_id, Some(right)));
                        }
                        Err(err) => {
                            tracing::error!(
                                ?new_leader_id,
                                error = &err as &dyn std::error::Error,
                                "failed to connect node to new leader, mesh is leaderless",
                            );
                            return;
                        }
                    }
                }
            }

            // Send all the followers to the new leader.
            let mut followers = Vec::new();
            for (remote_id, recv) in receivers {
                let send = senders
                    .remove(&remote_id)
                    .expect("should be in sync with receivers");
                followers.push((remote_id, recv, send));
            }
            new_leader_followers_sink.send(Followers { list: followers });
        } else {
            tracing::error!(?new_leader_id, "new leader is unknown, mesh is leaderless");
        }
    }
}

/// A task initiator, implementing by a function returning a future. This is
/// used to send work to the node's IO thread.
struct SmallTask {
    name: &'static str,
    future: BoxFuture<'static, ()>,
}

impl Debug for SmallTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("SmallTask")
    }
}

impl SmallTask {
    fn new(name: &'static str, f: impl 'static + Send + Future<Output = ()>) -> Self {
        Self {
            name,
            future: Box::pin(f),
        }
    }
}

/// An invitation allowing another process to join the mesh.
///
/// Created by [`UnixNode::invite`].
#[derive(Debug, Protobuf)]
#[mesh(resource = "Resource")]
pub struct Invitation {
    /// The common invitation addresses.
    pub address: InvitationAddress,
    /// The Unix socket used to initiate communications with the mesh.
    pub fd: OwnedFd,
}

#[derive(Debug)]
enum SenderCommand {
    Send {
        packet: Vec<u8>,
        fds: Vec<OsResource>,
    },
    ReleaseFds {
        count: usize,
    },
}

#[derive(Clone)]
struct PacketSender {
    send: mpsc::UnboundedSender<SenderCommand>,
    socket: Arc<UnixSocket>,
}

impl SendEvent for PacketSender {
    fn event(&self, event: OutgoingEvent<'_>) {
        let (packet, fds) = match serialize_event(event) {
            Ok(r) => r,
            Err(err) => {
                // FUTURE: fail the port or connection instead?
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "failed to serialize event"
                );
                return;
            }
        };

        // Try to send the packet immediately. If that is not possible (see
        // `try_send_now`) or fails (likely due to EAGAIN), send the packet to
        // the asynchronous task for deferred processing.
        //
        // N.B. This can lead to out of order messages. The event protocol is
        //      responsible for handling this condition.
        if !self.socket.try_send_now(&packet, &fds) {
            let _ = self
                .send
                .unbounded_send(SenderCommand::Send { packet, fds });
        }
    }
}

fn serialize_event(event: OutgoingEvent<'_>) -> io::Result<(Vec<u8>, Vec<OsResource>)> {
    // Serialize the event to a memfd if it's too large to send inline.
    if event.len() > MAX_SMALL_EVENT_SIZE {
        return serialize_large_event(event);
    }

    // Serialize the event to a byte vector.
    let cap = size_of::<protocol::PacketHeader>() + event.len();
    let mut packet = Vec::with_capacity(cap);
    packet.extend_from_slice(
        protocol::PacketHeader {
            packet_type: protocol::PacketType::EVENT,
            ..FromZeros::new_zeroed()
        }
        .as_bytes(),
    );
    let mut fds = Vec::new();
    event.write_to(&mut packet, &mut fds);
    assert_eq!(packet.len(), cap);
    Ok((packet, fds))
}

#[cfg(target_os = "linux")]
fn serialize_large_event(event: OutgoingEvent<'_>) -> io::Result<(Vec<u8>, Vec<OsResource>)> {
    let packet = protocol::PacketHeader {
        packet_type: protocol::PacketType::LARGE_EVENT,
        ..FromZeros::new_zeroed()
    }
    .as_bytes()
    .to_vec();

    let mut fds = Vec::new();

    let mut memfd = memfd::MemfdBuilder::new(event.len())?;
    event.write_to(&mut io::Cursor::new(&mut *memfd), &mut fds);
    fds.insert(0, OsResource::Fd(memfd.seal()?.into()));

    Ok((packet, fds))
}

#[cfg(not(target_os = "linux"))]
fn serialize_large_event(_event: OutgoingEvent<'_>) -> io::Result<(Vec<u8>, Vec<OsResource>)> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "event too large for this OS",
    ))
}

impl Drop for PacketSender {
    fn drop(&mut self) {
        // Explicitly close the send channel so that the send task returns, even
        // though the send channel is also in use by the receive task.
        self.send.close_channel();
    }
}

/// Starts a connection processing task.
fn start_connection(
    tasks: &mesh_channel::Sender<SmallTask>,
    local_node: &Arc<LocalNode>,
    remote_id: NodeId,
    handle: RemoteNodeHandle,
    socket: UnixSocket,
) {
    #[expect(clippy::disallowed_methods)] // TODO
    let (send, recv) = mpsc::unbounded();
    let socket = Arc::new(socket);
    let sender = PacketSender {
        send: send.clone(),
        socket: socket.clone(),
    };
    if handle.connect(sender) {
        let task = SmallTask::new("run_connection", {
            let local_node = local_node.clone();
            run_connection(local_node, remote_id, send, recv, socket, handle)
        });
        tasks.send(task);
        tracing::debug!(?remote_id, "connected");
    } else {
        // N.B. This is an expected condition in many scenarios, since the
        //      leader does not track which connections have already been
        //      made and so will often send duplicate connection requests.
        tracing::debug!(?remote_id, "duplicate connection");
    }
}

/// Runs the packet processing loop.
#[instrument(skip_all, fields(local_id = ?local_node.id(), remote_id = ?remote_id))]
async fn run_connection(
    local_node: Arc<LocalNode>,
    remote_id: NodeId,
    send_send: mpsc::UnboundedSender<SenderCommand>,
    send_recv: mpsc::UnboundedReceiver<SenderCommand>,
    socket: Arc<UnixSocket>,
    handle: RemoteNodeHandle,
) {
    let mut retained_fds = VecDeque::new();
    let mut recv = pin!(
        async {
            let r = run_receive(&local_node, &remote_id, &socket, &send_send).await;
            match &r {
                Ok(_) => {
                    tracing::debug!("incoming socket disconnected");
                }
                Err(err) => {
                    tracing::error!(error = err as &dyn std::error::Error, "error receiving");
                }
            }
            r
        }
        .fuse()
    );
    let mut send = pin!(
        async {
            match run_send(send_recv, &socket, &mut retained_fds).await {
                Ok(_) => {
                    tracing::debug!("sending is done");
                }
                Err(err) => {
                    tracing::error!(error = &err as &dyn std::error::Error, "failed send");
                }
            }
        }
        .fuse()
    );
    let r = futures::select! { // race semantics
        r = recv => {
            // Notify the remote node that no more data will be sent.
            tracing::trace!("read complete, shutting down writes");
            let _ = socket.close_write().await;
            r
        }
        _ = send => {
            match socket.close_write().await {
                Ok(()) => {
                    tracing::trace!("shutdown writes, waiting for reads");
                    recv.await
                }
                Err(err) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "failed to shutdown writes, aborting connection",
                    );
                    Err(ReceiveError::Io(err))
                }
            }
        }
    };
    tracing::trace!("connection done");
    match r {
        Ok(()) => handle.disconnect(),
        Err(err) => handle.fail(err),
    }
}

#[derive(Debug, Error)]
enum ReceiveError {
    #[error("i/o error")]
    Io(#[from] io::Error),
    #[error("missing packet header")]
    NoHeader,
    #[error("release fds packet too small")]
    BadReleaseFds,
    #[error("unknown packet type {0:?}")]
    UnknownPacketType(protocol::PacketType),
    #[error("file descriptor packet carries no file descriptors")]
    EmptyEventFds,
    #[error(
        "event carries more than {} file descriptors",
        protocol::MAX_FDS_PER_EVENT
    )]
    TooManyFds,
    #[error("event claims {claimed} file descriptors sent ahead, but {received} arrived")]
    FdsAheadMismatch { claimed: u32, received: usize },
    #[error(
        "connection closed with {0} file descriptors sent ahead of an event that never arrived"
    )]
    FdsPendingAtEof(usize),
    #[cfg(target_os = "linux")]
    #[error("memfd file descriptor not sent for large event")]
    MissingMemfd,
    #[cfg(target_os = "linux")]
    #[error("failed to map memfd")]
    Memfd(#[source] io::Error),
}

/// Handles receive processing for the socket.
async fn run_receive(
    local_node: &LocalNode,
    remote_id: &NodeId,
    socket: &UnixSocket,
    send: &mpsc::UnboundedSender<SenderCommand>,
) -> Result<(), ReceiveError> {
    let mut buf = vec![0; MAX_PACKET_SIZE];
    let mut fds = Vec::new();
    // Descriptors from `EVENT_FDS` packets, waiting for their event. Dropped,
    // and so closed, on every error return.
    let mut ahead = Vec::new();
    let mut receiver = unix_socket::ScmReceiver::new(protocol::MAX_FDS_PER_MESSAGE);
    loop {
        let len = socket.recv(&mut buf, &mut fds, &mut receiver).await?;
        if len == 0 {
            if !ahead.is_empty() {
                return Err(ReceiveError::FdsPendingAtEof(ahead.len()));
            }
            break;
        }
        if cfg!(target_os = "macos") && !fds.is_empty() {
            // Tell the opposite endpoint to release the fds it sent.
            let _ = send.unbounded_send(SenderCommand::Send {
                packet: protocol::ReleaseFds {
                    header: protocol::PacketHeader {
                        packet_type: protocol::PacketType::RELEASE_FDS,
                        ..FromZeros::new_zeroed()
                    },
                    count: fds.len() as u64,
                }
                .as_bytes()
                .to_vec(),
                fds: Vec::new(),
            });
        }

        let buf = &buf[..len];
        let header = protocol::PacketHeader::read_from_prefix(buf)
            .map_err(|_| ReceiveError::NoHeader)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
        match header.packet_type {
            protocol::PacketType::EVENT => {
                take_fds_ahead(&mut ahead, &mut fds, header.fds_ahead)?;
                local_node.event(remote_id, &buf[size_of_val(&header)..], &mut fds);
                fds.clear();
            }
            protocol::PacketType::EVENT_FDS => {
                if fds.is_empty() {
                    return Err(ReceiveError::EmptyEventFds);
                }
                if ahead.len() + fds.len() > protocol::MAX_FDS_PER_EVENT {
                    return Err(ReceiveError::TooManyFds);
                }
                ahead.append(&mut fds);
            }
            protocol::PacketType::RELEASE_FDS => {
                let release_fds = protocol::ReleaseFds::read_from_prefix(buf)
                    .map_err(|_| ReceiveError::BadReleaseFds)?
                    .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                let _ = send.unbounded_send(SenderCommand::ReleaseFds {
                    count: release_fds.count as usize,
                });
            }
            #[cfg(target_os = "linux")]
            protocol::PacketType::LARGE_EVENT => {
                // The memfd is the event's first descriptor, so with a split
                // event it arrives ahead.
                take_fds_ahead(&mut ahead, &mut fds, header.fds_ahead)?;
                if fds.is_empty() {
                    return Err(ReceiveError::MissingMemfd);
                }
                let OsResource::Fd(fd) = fds.remove(0);
                let memfd = memfd::SealedMemfd::new(fd.into()).map_err(ReceiveError::Memfd)?;
                local_node.event(remote_id, &memfd, &mut fds);
                fds.clear();
            }
            ty => {
                return Err(ReceiveError::UnknownPacketType(ty));
            }
        }
    }
    Ok(())
}

/// Moves the descriptors sent ahead of an event in front of the event's own,
/// failing if the event does not claim exactly that many.
fn take_fds_ahead(
    ahead: &mut Vec<OsResource>,
    fds: &mut Vec<OsResource>,
    claimed: u32,
) -> Result<(), ReceiveError> {
    if ahead.len() != claimed as usize {
        return Err(ReceiveError::FdsAheadMismatch {
            claimed,
            received: ahead.len(),
        });
    }
    fds.splice(..0, ahead.drain(..));
    Ok(())
}

#[derive(Debug, Error)]
enum ProtocolError {
    #[error("request to release too many fds")]
    ReleasingTooManyFds,
}

/// Handles send processing for the socket.
async fn run_send(
    mut recv: mpsc::UnboundedReceiver<SenderCommand>,
    socket: &UnixSocket,
    retained_fds: &mut VecDeque<OsResource>,
) -> io::Result<()> {
    while let Some(command) = recv.next().await {
        match command {
            SenderCommand::Send { mut packet, fds } => {
                match socket.send_packet(&mut packet, &fds).await {
                    Ok(_) => (),
                    Err(err) => {
                        tracing::error!(
                            fd_count = fds.len(),
                            packet_len = packet.len(),
                            "failed to send packet"
                        );
                        return Err(err);
                    }
                }
                if cfg!(target_os = "macos") {
                    // MacOS has a bug where it prematurely closes Unix sockets
                    // if a file descriptor to one is closed while it is also in
                    // the process of being sent across another Unix socket.
                    // Retain the fds until the opposite endpoint sends a reply
                    // message.
                    if !fds.is_empty() {
                        retained_fds.extend(fds);
                    }
                }
            }
            SenderCommand::ReleaseFds { count } => {
                if retained_fds.len() < count {
                    return Err(io::Error::other(ProtocolError::ReleasingTooManyFds));
                }
                retained_fds.drain(..count);
            }
        }
    }
    Ok(())
}

/// An offer to take over as leader of the mesh. One trusted process must be the
/// leader at all times or the mesh will fail.
#[derive(Debug, Protobuf)]
#[mesh(resource = "Resource")]
pub struct LeadershipOffer {
    send: mesh_channel::Sender<(NodeId, mesh_channel::Sender<Followers>)>,
}

impl UnixNode {
    /// Creates a new Unix node mesh with this node as the leader.
    pub fn new(driver: impl SpawnDriver) -> Self {
        let (to_leader_send, to_leader_recv) = channel();
        let (from_leader_send, from_leader_recv) = channel();
        let this = Self::with_id(
            Arc::new(driver),
            NodeId::new(),
            to_leader_send,
            from_leader_recv,
        );

        // Start a leader task. At this point the leader has only one follower, itself.
        let (resign_send, resign_recv) = channel();
        let resign_send = Arc::new(resign_send);
        let followers = Followers {
            list: vec![(this.local_node.id(), to_leader_recv, from_leader_send)],
        };
        let task = SmallTask::new("run_leader", {
            let local_node = this.local_node.clone();
            let tasks = this.tasks.clone();
            let driver = this.driver.clone();
            async move { run_leader(driver.as_ref(), &local_node, resign_recv, followers, &tasks).await }
        });
        this.tasks.send(task);
        *this.leader_resign_send.lock() = Some(resign_send);

        this
    }

    /// Gets the node ID. This is mostly useful for diagnostics.
    pub fn id(&self) -> NodeId {
        self.local_node.id()
    }

    /// Creates a node with `id` using `to_leader` and `from_leader` to
    /// communicate with the leader node.
    fn with_id(
        driver: Arc<dyn SpawnDriver>,
        id: NodeId,
        to_leader: mesh_channel::Sender<LeaderRequest>,
        from_leader: mesh_channel::Receiver<FollowerRequest>,
    ) -> Self {
        let to_leader = Arc::new(to_leader);
        let pending_connections: Arc<Mutex<HashMap<NodeId, RemoteNodeHandle>>> = Default::default();
        let local_node = Arc::new(LocalNode::with_id(
            id,
            Box::new(Connector {
                local_id: id,
                conn_req_send: to_leader.clone(),
                pending_connections: pending_connections.clone(),
            }),
        ));
        let (task_send, mut task_recv) = channel::<SmallTask>();
        let task_send = Arc::new(task_send);
        let (drop_send, drop_recv) = oneshot();

        // Start a thread to run IO tasks.
        let io_task = driver.spawn("unix-mesh-io", async move {
            let process = async {
                let mut futs = FuturesUnordered::new();
                loop {
                    futures::select! { // merge semantics
                        _ = futs.next() => {},
                        task = task_recv.select_next_some() => {
                            futs.push(async move {
                                tracing::trace!(?id, name = task.name, "task start");
                                task.future.await;
                                tracing::trace!(?id, name = task.name, "task end");
                            });
                        },
                        complete => break,
                    };
                }
            };
            future::select(pin!(process), drop_recv).await;
        });

        task_send.send(SmallTask::new("run_follower", {
            let local_node = local_node.clone();
            let tasks = task_send.clone();
            let driver = driver.clone();
            async move {
                run_follower(
                    driver.as_ref(),
                    &local_node,
                    from_leader,
                    pending_connections,
                    &tasks,
                )
                .await
            }
        }));

        Self {
            driver,
            local_node,
            tasks: task_send,
            io_task,
            to_leader,
            leader_resign_send: Mutex::new(None),

            _drop_send: drop_send,
        }
    }

    /// Returns an offer to hand the leadership to another node in the mesh.
    ///
    /// The offer should be sent over a channel and passed to
    /// `accept_leadership` in the receiving node.
    pub fn offer_leadership(&self) -> LeadershipOffer {
        let (send, mut recv) = channel();
        if let Some(leader_send) = self.leader_resign_send.lock().clone() {
            // Start a task to wait for the offer to be acknowledged, then send
            // the offer details to the leader thread.
            let task = SmallTask::new("offer_leadership", async move {
                if let Ok(r) = recv.recv().await {
                    leader_send.send(r);
                }
            });
            self.tasks.send(task);
        }
        LeadershipOffer { send }
    }

    /// Accepts a leadership offer, making this node the current leader.
    pub fn accept_leadership(&self, offer: LeadershipOffer) {
        let (send, mut recv) = channel();
        offer.send.send((self.local_node.id(), send));

        let (resign_send, resign_recv) = channel();
        let resign_send = Arc::new(resign_send);
        let task = SmallTask::new("accept_and_run_leader", {
            let local_node = self.local_node.clone();
            let tasks = self.tasks.clone();
            let driver = self.driver.clone();
            async move {
                if let Ok(followers) = recv.recv().await {
                    drop(recv);
                    run_leader(driver.as_ref(), &local_node, resign_recv, followers, &tasks).await
                }
            }
        });
        self.tasks.send(task);
        *self.leader_resign_send.lock() = Some(resign_send);
    }

    /// Invites another process to join the mesh, with `port` bridged with the
    /// original port.
    #[instrument(skip_all, fields(local_id = ?self.local_node.id()))]
    pub async fn invite(&self, port: Port) -> Result<Invitation, InviteError> {
        let (invitation_send, mut invitation_recv) = channel();
        self.to_leader
            .send(LeaderRequest::Invite(port, invitation_send));
        let invitation = invitation_recv.recv().await.map_err(|_| InviteError)?;
        tracing::debug!(
            invite_id = ?invitation.address.local_addr.node,
            "received invitation",
        );
        Ok(invitation)
    }

    /// Extract a [`UnixMeshInviter`] handle. Cheap — clones an Arc.
    ///
    /// The inviter can be used from any task to create mesh invitations
    /// without holding a reference to this node.
    pub(crate) fn inviter(&self) -> UnixMeshInviter {
        UnixMeshInviter {
            to_leader: (*self.to_leader).clone(),
        }
    }

    /// Joins an existing mesh via an invitation, briding `port` with the
    /// initial port.
    pub async fn join(
        driver: impl SpawnDriver,
        invitation: Invitation,
        port: Port,
    ) -> Result<Self, JoinError> {
        Self::join_generic(Arc::new(driver), invitation, port).await
    }

    #[instrument(skip_all, fields(local_id = ?invitation.address.local_addr.node, remote_id = ?invitation.address.remote_addr.node))]
    async fn join_generic(
        driver: Arc<dyn SpawnDriver>,
        invitation: Invitation,
        port: Port,
    ) -> Result<Self, JoinError> {
        let (to_leader_send, to_leader_recv) = channel();
        let (from_leader_send, from_leader_recv) = channel();
        let this = Self::with_id(
            driver,
            invitation.address.local_addr.node,
            to_leader_send,
            from_leader_recv,
        );

        let handle = this
            .local_node
            .add_remote(invitation.address.remote_addr.node);
        let init_recv = OneshotReceiver::<InitialMessage>::from(this.local_node.add_port(
            invitation.address.local_addr.port,
            invitation.address.remote_addr,
        ));

        start_connection(
            &this.tasks,
            &this.local_node,
            invitation.address.remote_addr.node,
            handle,
            UnixSocket::new(this.driver.as_ref(), invitation.fd.into()),
        );

        let init_message = init_recv.await.map_err(JoinError)?;
        to_leader_recv.bridge(init_message.leader_send);
        from_leader_send.bridge(init_message.follower_recv);
        port.bridge(init_message.user_port);

        Ok(this)
    }

    /// Shuts down the node, waiting for any sent messages to be sent to their
    /// destination.
    ///
    /// After this call, any active ports will no longer be able to receive
    /// messages.
    ///
    /// It is essential to call this before exiting a mesh process; until this
    /// returns, data loss could occur for other mesh nodes.
    pub async fn shutdown(mut self) {
        // Wait for any proxy ports to disassociate.
        self.local_node.wait_for_ports(false).await;
        // Drop all connections to the leader.
        drop(self.to_leader);
        self.local_node.drop_connector();
        // Terminate the leader task.
        self.leader_resign_send.get_mut().take();
        // Fail all nodes so that the send threads are dropped.
        self.local_node.fail_all_nodes();
        // Signal the IO task to tear down.
        drop(self.tasks);
        // Wait for the IO task.
        self.io_task.await;
    }
}

/// An error returned by [`UnixNode::join`].
#[derive(Debug, Error)]
#[error("failed to accept invitation")]
pub struct JoinError(#[source] RecvError);

/// The connector used when the mesh needs to connect to a previously-recognized
/// node. Sends a message to the leader node to get a new socket to communicate
/// over.
#[derive(Debug)]
struct Connector {
    local_id: NodeId,
    conn_req_send: Arc<mesh_channel::Sender<LeaderRequest>>,
    pending_connections: Arc<Mutex<HashMap<NodeId, RemoteNodeHandle>>>,
}

impl Connect for Connector {
    fn connect(&self, node_id: NodeId, handle: RemoteNodeHandle) {
        tracing::trace!(local_id = ?self.local_id, remote_id = ?node_id, "connecting");
        let old_request = self.pending_connections.lock().insert(node_id, handle);
        if old_request.is_some() {
            panic!("duplicate connection request for {:?}", node_id);
        }
        self.conn_req_send.send(LeaderRequest::Connect(node_id))
    }
}

/// Creates an AF_UNIX socket pair of the appropriate type.
fn new_socket_pair() -> Result<(Socket, Socket), io::Error> {
    let ty = if USE_SEQPACKET {
        socket2::Type::SEQPACKET
    } else {
        socket2::Type::STREAM
    };
    Socket::pair(socket2::Domain::UNIX, ty, None)
}

/// An AF_UNIX SOCK_SEQPACKET or SOCK_STREAM connection.
struct UnixSocket {
    socket: Mutex<PolledSocket<Socket>>,
    /// True for SOCK_SEQPACKET, whose packet boundaries the kernel keeps. On
    /// SOCK_STREAM each packet is prefixed with its length.
    seqpacket: bool,
    /// Set while the send task is between the `EVENT_FDS` packets of an event
    /// and the event packet. Only changed with `socket` locked.
    sending_split_event: AtomicBool,
}

impl UnixSocket {
    fn new(driver: &dyn SpawnDriver, fd: Socket) -> Self {
        // Frame by the socket's actual type rather than the platform default.
        let seqpacket = fd
            .r#type()
            .map_or(USE_SEQPACKET, |ty| ty == socket2::Type::SEQPACKET);
        let socket = PolledSocket::new(driver, fd).unwrap();
        UnixSocket {
            socket: Mutex::new(socket),
            seqpacket,
            sending_split_event: AtomicBool::new(false),
        }
    }

    /// Sends a whole packet from the calling thread without waiting, for the
    /// synchronous fast path. Returns false if the caller must hand the packet
    /// to the send task instead.
    fn try_send_now(&self, packet: &[u8], fds: &[OsResource]) -> bool {
        // On SOCK_STREAM a packet may take several writes, which must not
        // interleave with the send task's. A packet with more descriptors than
        // one message holds must be split, which only the send task does: two
        // writers splitting at once would mix their pieces.
        if !self.seqpacket || fds.len() > protocol::MAX_FDS_PER_MESSAGE {
            return false;
        }
        let socket = self.socket.lock();
        // The receiver gives the descriptors of `EVENT_FDS` packets to the next
        // event packet, so no packet may land between them. Relaxed is enough:
        // the flag only changes with the lock held.
        if self.sending_split_event.load(Ordering::Relaxed) {
            return false;
        }
        unix_socket::send_with_fds(
            socket.get().as_fd(),
            &[IoSlice::new(packet)],
            fds.iter().map(|OsResource::Fd(fd)| fd.as_fd()),
        )
        .is_ok()
    }

    fn set_sending_split_event(&self, value: bool) {
        let _socket = self.socket.lock();
        self.sending_split_event.store(value, Ordering::Relaxed);
    }

    /// Sends a packet from the send task. An event with more descriptors than
    /// one message holds sends all but the last chunk in `EVENT_FDS` packets,
    /// then the event packet with `fds_ahead` set to how many went ahead.
    ///
    /// On SOCK_STREAM descriptors arrive with the first byte of their write, and
    /// the receiver never reads past the end of a packet, so they stay with it.
    async fn send_packet(&self, packet: &mut [u8], fds: &[OsResource]) -> io::Result<()> {
        if fds.len() <= protocol::MAX_FDS_PER_MESSAGE {
            return self.send(packet, fds).await;
        }
        if fds.len() > protocol::MAX_FDS_PER_EVENT {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "too many file descriptors for one event ({}, max {})",
                    fds.len(),
                    protocol::MAX_FDS_PER_EVENT
                ),
            ));
        }
        let fds_ahead =
            (fds.len() - 1) / protocol::MAX_FDS_PER_MESSAGE * protocol::MAX_FDS_PER_MESSAGE;
        let (ahead, last) = fds.split_at(fds_ahead);
        let (mut header, _) = protocol::PacketHeader::read_from_prefix(packet)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "packet has no header"))?;
        header.fds_ahead = fds_ahead as u32;
        header
            .write_to_prefix(packet)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "packet has no header"))?;
        let fds_header = protocol::PacketHeader {
            packet_type: protocol::PacketType::EVENT_FDS,
            ..FromZeros::new_zeroed()
        };

        // Keep the fast path off the socket until the event packet is out. If
        // a send fails the flag stays set: the connection is going down, and
        // the receiver would refuse anything else after a partial split.
        self.set_sending_split_event(true);
        for chunk in ahead.chunks(protocol::MAX_FDS_PER_MESSAGE) {
            self.send(fds_header.as_bytes(), chunk).await?;
        }
        self.send(packet, last).await?;
        self.set_sending_split_event(false);
        Ok(())
    }

    async fn send(&self, msg: &[u8], fds: &[OsResource]) -> io::Result<()> {
        if self.seqpacket {
            self.send_raw(&mut [IoSlice::new(msg)], fds).await?;
        } else {
            let len = (msg.len() as u32).to_le_bytes();
            let mut iov = [IoSlice::new(&len), IoSlice::new(msg)];
            self.send_all_raw(&mut iov, fds).await?;
        }
        Ok(())
    }

    async fn send_raw(
        &self,
        iov: &mut [IoSlice<'_>],
        fds: &[OsResource],
    ) -> Result<usize, io::Error> {
        // Bound the fd count to what the receiver can hold. Without this,
        // attaching more than `MAX_FDS_PER_MESSAGE` descriptors would
        // deterministically trip `MSG_CTRUNC` on the receiver and tear down the
        // connection; reject it locally with a clear error instead.
        if fds.len() > protocol::MAX_FDS_PER_MESSAGE {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "too many file descriptors ({}, max {})",
                    fds.len(),
                    protocol::MAX_FDS_PER_MESSAGE
                ),
            ));
        }
        let n = poll_fn(|cx| {
            self.socket
                .lock()
                .poll_io(cx, InterestSlot::Write, PollEvents::OUT, |socket| {
                    unix_socket::send_with_fds(
                        socket.get().as_fd(),
                        iov,
                        fds.iter().map(|OsResource::Fd(fd)| fd.as_fd()),
                    )
                })
        })
        .await?;
        Ok(n)
    }

    async fn send_all_raw(
        &self,
        mut iov: &mut [IoSlice<'_>],
        mut fds: &[OsResource],
    ) -> Result<(), io::Error> {
        while !iov.is_empty() || !fds.is_empty() {
            let n = self.send_raw(iov, fds).await?;
            IoSlice::advance_slices(&mut iov, n);
            fds = &[];
        }
        Ok(())
    }

    async fn recv(
        &self,
        buf: &mut [u8],
        fds: &mut Vec<OsResource>,
        receiver: &mut unix_socket::ScmReceiver,
    ) -> io::Result<usize> {
        if self.seqpacket {
            self.recv_raw(buf, fds, receiver).await
        } else {
            let mut len = [0; 4];
            if !self.recv_all_raw(&mut len, fds, receiver).await? {
                return Ok(0);
            }
            let len = u32::from_le_bytes(len) as usize;
            let buf = buf
                .get_mut(..len)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EMSGSIZE))?;
            if !self.recv_all_raw(buf, fds, receiver).await? {
                return Err(ErrorKind::UnexpectedEof.into());
            }
            Ok(len)
        }
    }

    async fn recv_all_raw(
        &self,
        buf: &mut [u8],
        fds: &mut Vec<OsResource>,
        receiver: &mut unix_socket::ScmReceiver,
    ) -> Result<bool, io::Error> {
        let mut read = 0;
        while read < buf.len() {
            let n = self.recv_raw(&mut buf[read..], fds, receiver).await?;
            if n == 0 {
                if read != 0 {
                    return Err(ErrorKind::UnexpectedEof.into());
                } else {
                    return Ok(false);
                }
            }
            read += n;
        }
        Ok(true)
    }

    async fn recv_raw(
        &self,
        buf: &mut [u8],
        fds: &mut Vec<OsResource>,
        receiver: &mut unix_socket::ScmReceiver,
    ) -> Result<usize, io::Error> {
        let n = poll_fn(|cx| {
            self.socket
                .lock()
                .poll_io(cx, InterestSlot::Read, PollEvents::IN, |socket| {
                    receiver.recv(socket.get().as_fd(), buf)
                })
        })
        .await?;
        fds.extend(receiver.drain().map(OsResource::Fd));
        Ok(n)
    }

    async fn close_write(&self) -> io::Result<()> {
        self.socket.lock().get().shutdown(std::net::Shutdown::Write)
    }
}

#[cfg(test)]
mod tests {
    use crate::unix::UnixNode;
    use mesh_channel::RecvError;
    use mesh_channel::channel;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;

    #[async_test]
    async fn test_basic(driver: DefaultDriver) {
        let leader = UnixNode::new(driver.clone());
        let (send, recv) = channel::<u32>();
        let invitation = leader.invite(recv.into()).await.unwrap();
        let (send2, mut recv2) = channel::<u32>();
        let follower = UnixNode::join(driver, invitation, send2.into())
            .await
            .unwrap();
        send.send(5);
        assert_eq!(recv2.recv().await.unwrap(), 5);
        drop(send);
        drop(recv2);
        follower.shutdown().await;
        leader.shutdown().await;
    }

    #[cfg(target_os = "linux")]
    #[async_test]
    async fn test_huge_message(driver: DefaultDriver) {
        let leader = UnixNode::new(driver.clone());
        let (send, recv) = channel::<Vec<u8>>();
        let invitation = leader.invite(recv.into()).await.unwrap();
        let (send2, mut recv2) = channel::<Vec<u8>>();
        let follower = UnixNode::join(driver, invitation, send2.into())
            .await
            .unwrap();

        let v = vec![0xcc; 16 << 20];
        send.send(v.clone());
        let v2 = recv2.recv().await.unwrap();
        assert_eq!(v, v2);
        follower.shutdown().await;
        leader.shutdown().await;
    }

    #[cfg(target_os = "linux")]
    #[async_test]
    async fn test_message_sizes(driver: DefaultDriver) {
        let (p1, p2) = mesh_node::local_node::Port::new_pair();
        let (p3, p4) = mesh_node::local_node::Port::new_pair();
        let node1 = UnixNode::new(driver.clone());
        let invitation = node1.invite(p2).await.unwrap();
        let _node2 = UnixNode::join(driver.clone(), invitation, p3)
            .await
            .unwrap();

        crate::test_common::test_message_sizes(p1, p4, 0..=super::MAX_SMALL_EVENT_SIZE + 0x1000)
            .await;
    }

    #[async_test]
    async fn test_dropped_shutdown(driver: DefaultDriver) {
        let leader = UnixNode::new(driver.clone());
        {
            let (_send, recv) = channel::<u32>();
            let invitation = leader.invite(recv.into()).await.unwrap();
            let (send2, _recv2) = channel::<u32>();
            let _follower = UnixNode::join(driver, invitation, send2.into())
                .await
                .unwrap();
        }
        leader.shutdown().await;
    }

    #[async_test]
    async fn test_send_shutdown(driver: DefaultDriver) {
        let leader = UnixNode::new(driver.clone());
        let (send, mut recv) = channel::<u32>();
        let invitation = leader.invite(send.into()).await.unwrap();
        let (send2, recv2) = channel::<u32>();
        let follower = UnixNode::join(driver, invitation, recv2.into())
            .await
            .unwrap();
        send2.send(5);
        drop(send2);
        follower.shutdown().await;
        assert_eq!(recv.recv().await.unwrap(), 5);
    }

    #[async_test]
    async fn test_failed_invitation(driver: DefaultDriver) {
        let leader = UnixNode::new(driver);
        let (send, mut recv) = channel::<()>();
        leader.invite(send.into()).await.unwrap();
        assert!(matches!(
            recv.recv().await.unwrap_err(),
            RecvError::Error(_)
        ));
        drop(recv);
        leader.shutdown().await;
    }

    #[async_test]
    async fn test_three(driver: DefaultDriver) {
        let (p1, p2) = channel::<u32>();
        let (p3, mut p4) = channel::<u32>();
        let (p5, p6) = channel::<u32>();
        let (p7, p8) = channel::<u32>();

        let node1 = UnixNode::new(driver.clone());

        let invitation = node1.invite(p2.into()).await.unwrap();
        let node2 = UnixNode::join(driver.clone(), invitation, p3.into())
            .await
            .unwrap();

        let invitation = node1.invite(p5.into()).await.unwrap();
        let node3 = UnixNode::join(driver, invitation, p8.into()).await.unwrap();

        p1.bridge(p6);

        p7.send(5);

        assert_eq!(p4.recv().await.unwrap(), 5);
        drop(p4);
        drop(p7);
        futures::join!(node2.shutdown(), node3.shutdown());
        node1.shutdown().await;
    }

    #[async_test]
    async fn test_handoff_leader(driver: DefaultDriver) {
        let (p1, p2) = channel::<u32>();
        let (p3, p4) = channel::<u32>();
        let (p5, p6) = channel::<u32>();
        let (p7, p8) = channel::<u32>();
        let (p9, p10) = channel();
        let (p11, mut p12) = channel();

        let node1 = UnixNode::new(driver.clone());

        let invitation = node1.invite(p2.into()).await.unwrap();
        let node2 = UnixNode::join(driver.clone(), invitation, p3.into())
            .await
            .unwrap();

        let invitation = node1.invite(p5.into()).await.unwrap();
        let node3 = UnixNode::join(driver.clone(), invitation, p8.into())
            .await
            .unwrap();

        let invitation = node1.invite(p10.into()).await.unwrap();
        let node4 = UnixNode::join(driver, invitation, p11.into())
            .await
            .unwrap();

        p9.send(node1.offer_leadership());
        node4.accept_leadership(p12.recv().await.unwrap());
        drop(p9);
        drop(p12);
        p1.bridge(p6);

        std::thread::sleep(std::time::Duration::from_millis(200));

        node1.shutdown().await;
        drop(p4);
        drop(p7);
        node2.shutdown().await;
        node3.shutdown().await;

        std::thread::sleep(std::time::Duration::from_millis(200));

        node4.shutdown().await;
    }

    /// Events whose file descriptors do not fit in one message.
    ///
    /// These build connections directly over a socket pair (rather than via
    /// `UnixNode`) so that both `SOCK_SEQPACKET` and `SOCK_STREAM` framing can
    /// be exercised on Linux, and so that a raw peer can inject packets.
    #[cfg(target_os = "linux")]
    mod many_fds {
        use crate::unix_node::MAX_SMALL_EVENT_SIZE;
        use crate::unix_node::SmallTask;
        use crate::unix_node::UnixSocket;
        use crate::unix_node::start_connection;
        use futures::StreamExt;
        use mesh_channel::Receiver;
        use mesh_channel::Sender;
        use mesh_channel::channel;
        use mesh_node::common::Address;
        use mesh_node::common::NodeId;
        use mesh_node::common::PortId;
        use mesh_node::local_node::Connect;
        use mesh_node::local_node::LocalNode;
        use mesh_node::local_node::RemoteNodeHandle;
        use mesh_node::message::MeshField;
        use mesh_node::resource::OsResource;
        use mesh_node::resource::Resource;
        use mesh_protobuf::Protobuf;
        use pal_async::DefaultDriver;
        use pal_async::async_test;
        use pal_async::task::Spawn;
        use pal_async::task::Task;
        use socket2::Domain;
        use socket2::Socket;
        use socket2::Type;
        use std::fs::File;
        use std::io::ErrorKind;
        use std::io::IoSlice;
        use std::io::PipeWriter;
        use std::io::Write;
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::prelude::*;
        use std::pin::pin;
        use std::sync::Arc;
        use test_with_tracing::test;
        use unicycle::FuturesUnordered;

        const SOCKET_TYPES: [Type; 2] = [Type::SEQPACKET, Type::STREAM];

        // Wire values, written out rather than taken from `protocol` so these
        // tests also pin the on-the-wire layout.
        const EVENT: u8 = 1;
        const LARGE_EVENT: u8 = 3;
        const EVENT_FDS: u8 = 4;
        const MAX_FDS_PER_EVENT: usize = 4096;

        /// Raises the soft open-file limit: some tests hold thousands of
        /// descriptors at once (sent, in flight, and received), more than the
        /// common default soft limit of 1024.
        // UNSAFETY: libc calls to read and set the open-file limit.
        #[expect(unsafe_code)]
        fn raise_fd_limit() {
            const WANT: libc::rlim_t = 65536;
            // SAFETY: `rlimit` is plain data; a zeroed value is valid.
            let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
            // SAFETY: passing a valid pointer to an `rlimit`.
            if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
                return;
            }
            let want = WANT.min(limit.rlim_max);
            if limit.rlim_cur < want {
                limit.rlim_cur = want;
                // SAFETY: passing a valid pointer to an initialized `rlimit`.
                unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
            }
        }

        /// Formats an error with its whole source chain, where the transport's
        /// own failure reason lives.
        fn error_chain(err: &dyn std::error::Error) -> String {
            let mut s = err.to_string();
            let mut source = err.source();
            while let Some(err) = source {
                s.push_str(": ");
                s.push_str(&err.to_string());
                source = err.source();
            }
            s
        }

        /// The file a descriptor refers to; equal only for the same open file.
        fn file_id(fd: BorrowedFd<'_>) -> (u64, u64) {
            let meta = File::from(fd.try_clone_to_owned().unwrap())
                .metadata()
                .unwrap();
            (meta.dev(), meta.ino())
        }

        /// Makes `n` pipes, each with its own inode. Returns the read ends (to
        /// send) and the write ends (to keep).
        fn pipes(n: usize) -> (Vec<OwnedFd>, Vec<PipeWriter>) {
            (0..n)
                .map(|_| {
                    let (r, w) = std::io::pipe().unwrap();
                    (OwnedFd::from(r), w)
                })
                .unzip()
        }

        /// Asserts that every read end of these pipes has been closed, in the
        /// receiving node too: a write then fails with EPIPE (pipe(7)).
        fn assert_read_ends_closed(writers: Vec<PipeWriter>) {
            for mut w in writers {
                let err = w.write(b"x").expect_err("a read end is still open");
                assert_eq!(err.kind(), ErrorKind::BrokenPipe);
            }
        }

        struct NoConnect;

        impl Connect for NoConnect {
            fn connect(&self, _node_id: NodeId, _handle: RemoteNodeHandle) {}
        }

        fn new_node() -> Arc<LocalNode> {
            Arc::new(LocalNode::with_id(NodeId::new(), Box::new(NoConnect)))
        }

        /// Runs the connection tasks that `start_connection` hands off, as
        /// `UnixNode`'s IO task does.
        struct TestIo {
            tasks: Sender<SmallTask>,
            _task: Task<()>,
        }

        impl TestIo {
            fn new(driver: &DefaultDriver) -> Self {
                let (tasks, mut recv) = channel::<SmallTask>();
                let task = driver.spawn("test-io", async move {
                    let mut futs = FuturesUnordered::new();
                    loop {
                        futures::select! { // merge semantics
                            _ = futs.next() => {},
                            task = recv.select_next_some() => {
                                futs.push(task.future);
                            }
                            complete => break,
                        }
                    }
                });
                Self { tasks, _task: task }
            }

            fn connect(
                &self,
                driver: &DefaultDriver,
                node: &Arc<LocalNode>,
                remote_id: NodeId,
                socket: Socket,
            ) {
                let handle = node.add_remote(remote_id);
                start_connection(
                    &self.tasks,
                    node,
                    remote_id,
                    handle,
                    UnixSocket::new(driver, socket),
                );
            }
        }

        /// Two nodes connected by one socket pair of the given type.
        struct Pair {
            a: Arc<LocalNode>,
            b: Arc<LocalNode>,
            _io: TestIo,
        }

        impl Pair {
            fn new(driver: &DefaultDriver, ty: Type) -> Self {
                let (sa, sb) = Socket::pair(Domain::UNIX, ty, None).unwrap();
                let io = TestIo::new(driver);
                let a = new_node();
                let b = new_node();
                io.connect(driver, &a, b.id(), sa);
                io.connect(driver, &b, a.id(), sb);
                Self { a, b, _io: io }
            }

            /// A channel from node `a` to node `b`.
            fn channel<T: 'static + MeshField + Send>(&self) -> (Sender<T>, Receiver<T>) {
                let pa = PortId::new();
                let pb = PortId::new();
                let send = self.a.add_port(
                    pa,
                    Address {
                        node: self.b.id(),
                        port: pb,
                    },
                );
                let recv = self.b.add_port(
                    pb,
                    Address {
                        node: self.a.id(),
                        port: pa,
                    },
                );
                (send.into(), recv.into())
            }
        }

        #[derive(Protobuf)]
        #[mesh(resource = "Resource")]
        struct FdBatch {
            seq: u32,
            payload: Vec<u8>,
            fds: Vec<OwnedFd>,
        }

        /// Sends one event carrying `fd_count` descriptors and `payload_len`
        /// bytes, and checks that the same files arrive in the same order.
        async fn round_trip(driver: &DefaultDriver, ty: Type, fd_count: usize, payload_len: usize) {
            let pair = Pair::new(driver, ty);
            let (send, mut recv) = pair.channel::<FdBatch>();
            let (readers, _writers) = pipes(fd_count);
            let ids: Vec<_> = readers.iter().map(|fd| file_id(fd.as_fd())).collect();
            let payload: Vec<u8> = (0..payload_len).map(|i| i as u8).collect();
            send.send(FdBatch {
                seq: 7,
                payload: payload.clone(),
                fds: readers,
            });
            let got = recv
                .recv()
                .await
                .unwrap_or_else(|err| panic!("{ty:?}, {fd_count} fds: {}", error_chain(&err)));
            assert_eq!(got.seq, 7);
            assert!(got.payload == payload, "{ty:?}: payload differs");
            let got_ids: Vec<_> = got.fds.iter().map(|fd| file_id(fd.as_fd())).collect();
            assert!(
                got_ids == ids,
                "{ty:?}, {fd_count} fds: files differ or are out of order"
            );
        }

        async fn round_trip_all(driver: &DefaultDriver, fd_count: usize, payload_len: usize) {
            raise_fd_limit();
            for ty in SOCKET_TYPES {
                round_trip(driver, ty, fd_count, payload_len).await;
            }
        }

        #[async_test]
        async fn fds_0(driver: DefaultDriver) {
            round_trip_all(&driver, 0, 16).await;
        }

        #[async_test]
        async fn fds_1(driver: DefaultDriver) {
            round_trip_all(&driver, 1, 16).await;
        }

        #[async_test]
        async fn fds_64(driver: DefaultDriver) {
            round_trip_all(&driver, 64, 16).await;
        }

        #[async_test]
        async fn fds_65(driver: DefaultDriver) {
            round_trip_all(&driver, 65, 16).await;
        }

        #[async_test]
        async fn fds_128(driver: DefaultDriver) {
            round_trip_all(&driver, 128, 16).await;
        }

        #[async_test]
        async fn fds_254(driver: DefaultDriver) {
            round_trip_all(&driver, 254, 16).await;
        }

        #[async_test]
        async fn fds_600(driver: DefaultDriver) {
            round_trip_all(&driver, 600, 16).await;
        }

        /// The most descriptors one message can carry: an event this size is
        /// too big to send inline, and its memfd counts towards the cap too.
        #[async_test]
        async fn fds_at_cap(driver: DefaultDriver) {
            round_trip_all(&driver, MAX_FDS_PER_EVENT - 1, 16).await;
        }

        #[async_test]
        async fn send_refuses_fds_over_cap(driver: DefaultDriver) {
            raise_fd_limit();
            for ty in SOCKET_TYPES {
                let (a, _b) = Socket::pair(Domain::UNIX, ty, None).unwrap();
                let socket = UnixSocket::new(&driver, a);
                let (readers, _writers) = pipes(1);
                let fds: Vec<_> = (0..MAX_FDS_PER_EVENT + 1)
                    .map(|_| OsResource::Fd(readers[0].try_clone().unwrap()))
                    .collect();
                let mut packet = [EVENT, 0, 0, 0, 0, 0, 0, 0];
                let err = socket
                    .send_packet(&mut packet, &fds)
                    .await
                    .expect_err("sending should have been refused");
                assert_eq!(err.kind(), ErrorKind::InvalidInput, "{ty:?}: {err}");
                assert!(
                    err.to_string()
                        .contains("too many file descriptors for one event"),
                    "{ty:?}: {err}"
                );
            }
        }

        /// While the send task is stuck between the pieces of a split event,
        /// the synchronous fast path must not send, even with room on the
        /// socket: the receiver would pair the pieces with the wrong event.
        #[async_test]
        async fn fast_path_waits_for_split_event(driver: DefaultDriver) {
            raise_fd_limit();
            let (a, raw) = Socket::pair(Domain::UNIX, Type::SEQPACKET, None).unwrap();
            raw.set_nonblocking(true).unwrap();
            let socket = UnixSocket::new(&driver, a);
            let mut receiver = unix_socket::ScmReceiver::new(64);
            let mut buf = [0u8; 64];
            let mut read_packet = |packets: &mut Vec<(u8, u32, usize)>| -> bool {
                match receiver.recv(raw.as_fd(), &mut buf) {
                    Ok(n) => {
                        assert_eq!(n, 8);
                        let fds_ahead = u32::from_ne_bytes(buf[4..8].try_into().unwrap());
                        packets.push((buf[0], fds_ahead, receiver.fds().len()));
                        true
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => false,
                    Err(err) => panic!("{err}"),
                }
            };

            // Fill the socket with fd-less filler packets until it refuses
            // more, then make room for exactly two.
            let filler = [EVENT, 0, 0, 0, 0, 0, 0, 0];
            let mut fillers = 0;
            while socket.try_send_now(&filler, &[]) {
                fillers += 1;
            }
            let mut packets = Vec::new();
            assert!(read_packet(&mut packets) && read_packet(&mut packets));

            // A split event of five pieces gets two out, then waits.
            let (readers, _writers) = pipes(1);
            let dup = || OsResource::Fd(readers[0].try_clone().unwrap());
            let fds: Vec<_> = (0..300).map(|_| dup()).collect();
            let mut packet = [EVENT, 0, 0, 0, 0, 0, 0, 0];
            let mut split = pin!(socket.send_packet(&mut packet, &fds));
            assert!(
                futures::poll!(split.as_mut()).is_pending(),
                "setup: the split event should not fit"
            );

            // With room again, the fast path still holds back.
            assert!(read_packet(&mut packets));
            assert!(!socket.try_send_now(&filler, &[dup()]));

            while read_packet(&mut packets) {}
            split.await.unwrap();
            while read_packet(&mut packets) {}

            let rest: Vec<_> = packets.into_iter().skip(fillers).collect();
            assert_eq!(
                rest,
                [
                    (EVENT_FDS, 0, 64),
                    (EVENT_FDS, 0, 64),
                    (EVENT_FDS, 0, 64),
                    (EVENT_FDS, 0, 64),
                    (EVENT, 256, 44),
                ]
            );
        }

        #[async_test]
        async fn large_event_1_fd(driver: DefaultDriver) {
            round_trip_all(&driver, 1, MAX_SMALL_EVENT_SIZE + 0x1000).await;
        }

        #[async_test]
        async fn large_event_300_fds(driver: DefaultDriver) {
            round_trip_all(&driver, 300, MAX_SMALL_EVENT_SIZE + 0x1000).await;
        }

        /// A split event sent by the connection's send task while other
        /// threads send small events through the synchronous fast path: no
        /// small event may land between the pieces of a split one.
        #[async_test]
        async fn split_events_race_fast_path(driver: DefaultDriver) {
            const BIG_EVENTS: u32 = 8;
            const BIG_FDS: usize = 600;
            const SMALL_EVENTS: u32 = 2000;

            raise_fd_limit();
            let pair = Pair::new(&driver, Type::SEQPACKET);
            let (big_send, mut big_recv) = pair.channel::<FdBatch>();
            let (small_send, mut small_recv) = pair.channel::<FdBatch>();
            let (big, _big_writers) = pipes(BIG_FDS);
            let big_ids: Vec<_> = big.iter().map(|fd| file_id(fd.as_fd())).collect();
            let (small, _small_writers) = pipes(1);
            let small_ids: Vec<_> = small.iter().map(|fd| file_id(fd.as_fd())).collect();

            let dup = |fds: &[OwnedFd]| -> Vec<OwnedFd> {
                fds.iter().map(|fd| fd.try_clone().unwrap()).collect()
            };
            let big_thread = std::thread::spawn(move || {
                for seq in 0..BIG_EVENTS {
                    big_send.send(FdBatch {
                        seq,
                        payload: Vec::new(),
                        fds: dup(&big),
                    });
                }
            });
            let small_thread = std::thread::spawn(move || {
                for seq in 0..SMALL_EVENTS {
                    small_send.send(FdBatch {
                        seq,
                        payload: Vec::new(),
                        fds: dup(&small),
                    });
                }
            });

            let check = async |recv: &mut Receiver<FdBatch>, count: u32, ids: &[(u64, u64)]| {
                for seq in 0..count {
                    let got = recv
                        .recv()
                        .await
                        .unwrap_or_else(|err| panic!("event {seq}: {}", error_chain(&err)));
                    assert_eq!(got.seq, seq);
                    let got_ids: Vec<_> = got.fds.iter().map(|fd| file_id(fd.as_fd())).collect();
                    assert!(
                        got_ids == ids,
                        "event {seq}: files differ or are out of order"
                    );
                }
            };
            futures::future::join(
                check(&mut big_recv, BIG_EVENTS, &big_ids),
                check(&mut small_recv, SMALL_EVENTS, &small_ids),
            )
            .await;
            big_thread.join().unwrap();
            small_thread.join().unwrap();
        }

        /// Sends a header-only packet: the type byte, three reserved bytes,
        /// then `fds_ahead` as a native-endian u32.
        fn send_raw_packet(
            raw: &Socket,
            ty: Type,
            packet_type: u8,
            fds_ahead: u32,
            fds: &[OwnedFd],
        ) {
            let mut header = [0u8; 8];
            header[0] = packet_type;
            header[4..].copy_from_slice(&fds_ahead.to_ne_bytes());
            let len = (header.len() as u32).to_le_bytes();
            let iov = if ty == Type::SEQPACKET {
                vec![IoSlice::new(&header)]
            } else {
                vec![IoSlice::new(&len), IoSlice::new(&header)]
            };
            let total: usize = iov.iter().map(|b| b.len()).sum();
            let n = unix_socket::send_with_fds(raw.as_fd(), &iov, fds.iter().map(|fd| fd.as_fd()))
                .unwrap();
            assert_eq!(n, total);
        }

        /// A node connected to a raw socket, so tests can send packets a
        /// well-behaved sender never would.
        struct RawPeer {
            raw: Socket,
            ty: Type,
            recv: Receiver<u32>,
            _node: Arc<LocalNode>,
            _io: TestIo,
        }

        impl RawPeer {
            fn new(driver: &DefaultDriver, ty: Type) -> Self {
                let (s, raw) = Socket::pair(Domain::UNIX, ty, None).unwrap();
                let io = TestIo::new(driver);
                let node = new_node();
                let raw_id = NodeId::new();
                io.connect(driver, &node, raw_id, s);
                // A port peered with the raw side: it fails when the
                // connection does.
                let port = node.add_port(
                    PortId::new(),
                    Address {
                        node: raw_id,
                        port: PortId::new(),
                    },
                );
                Self {
                    raw,
                    ty,
                    recv: port.into(),
                    _node: node,
                    _io: io,
                }
            }

            fn send(&self, packet_type: u8, fds_ahead: u32, fds: &[OwnedFd]) {
                send_raw_packet(&self.raw, self.ty, packet_type, fds_ahead, fds);
            }

            /// Waits for the connection to fail and checks why.
            async fn expect_failure(&mut self, reason: &str) {
                let err = self
                    .recv
                    .recv()
                    .await
                    .expect_err("connection should have failed");
                let chain = error_chain(&err);
                assert!(
                    chain.contains(reason),
                    "{:?}: expected {reason:?}, got: {chain}",
                    self.ty
                );
            }
        }

        /// Sends `sent` descriptors ahead, then an event of `event_type` that
        /// claims `claimed` of them.
        async fn fds_ahead_mismatch(
            driver: &DefaultDriver,
            event_type: u8,
            sent: usize,
            claimed: u32,
        ) {
            for ty in SOCKET_TYPES {
                let mut peer = RawPeer::new(driver, ty);
                let (readers, writers) = pipes(sent);
                if !readers.is_empty() {
                    peer.send(EVENT_FDS, 0, &readers);
                }
                drop(readers);
                peer.send(event_type, claimed, &[]);
                peer.expect_failure(&format!(
                    "event claims {claimed} file descriptors sent ahead, but {sent} arrived"
                ))
                .await;
                assert_read_ends_closed(writers);
            }
        }

        #[async_test]
        async fn refuses_claim_of_more_than_sent(driver: DefaultDriver) {
            fds_ahead_mismatch(&driver, EVENT, 2, 3).await;
        }

        #[async_test]
        async fn refuses_claim_of_fewer_than_sent(driver: DefaultDriver) {
            fds_ahead_mismatch(&driver, EVENT, 2, 1).await;
        }

        #[async_test]
        async fn refuses_unclaimed_fds(driver: DefaultDriver) {
            fds_ahead_mismatch(&driver, EVENT, 1, 0).await;
        }

        #[async_test]
        async fn refuses_claim_with_nothing_ahead(driver: DefaultDriver) {
            fds_ahead_mismatch(&driver, EVENT, 0, 1).await;
        }

        #[async_test]
        async fn refuses_large_event_mismatch(driver: DefaultDriver) {
            fds_ahead_mismatch(&driver, LARGE_EVENT, 2, 3).await;
        }

        #[async_test]
        async fn refuses_empty_fds_packet(driver: DefaultDriver) {
            for ty in SOCKET_TYPES {
                let mut peer = RawPeer::new(&driver, ty);
                peer.send(EVENT_FDS, 0, &[]);
                peer.expect_failure("carries no file descriptors").await;
            }
        }

        #[async_test]
        async fn refuses_fds_over_cap(driver: DefaultDriver) {
            raise_fd_limit();
            for ty in SOCKET_TYPES {
                let mut peer = RawPeer::new(&driver, ty);
                let (readers, writers) = pipes(1);
                // Copies of one descriptor: each arrives as its own new fd.
                let chunk: Vec<OwnedFd> =
                    (0..64).map(|_| readers[0].try_clone().unwrap()).collect();
                drop(readers);
                // Send from another thread: the blocking socket can fill up,
                // and the receiving node runs on this one.
                let raw = peer.raw.try_clone().unwrap();
                let thread = std::thread::spawn(move || {
                    for _ in 0..MAX_FDS_PER_EVENT / chunk.len() + 1 {
                        send_raw_packet(&raw, ty, EVENT_FDS, 0, &chunk);
                    }
                });
                peer.expect_failure(&format!("more than {MAX_FDS_PER_EVENT} file descriptors"))
                    .await;
                thread.join().unwrap();
                assert_read_ends_closed(writers);
            }
        }

        #[async_test]
        async fn closes_pending_fds_when_peer_drops(driver: DefaultDriver) {
            for ty in SOCKET_TYPES {
                let mut peer = RawPeer::new(&driver, ty);
                let (readers, writers) = pipes(2);
                peer.send(EVENT_FDS, 0, &readers);
                drop(readers);
                peer.raw.shutdown(std::net::Shutdown::Write).unwrap();
                peer.expect_failure("connection closed with 2 file descriptors sent ahead")
                    .await;
                assert_read_ends_closed(writers);
            }
        }
    }
}
