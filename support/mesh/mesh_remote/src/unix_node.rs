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
//! of Unix sockets.

#![cfg(unix)]
// UNSAFETY: Calls to libc send/recvmsg fns and the work to prepare their inputs
// and handle their outputs (mem::zeroed, transmutes, from_raw_fds).
#![expect(unsafe_code)]

#[cfg(target_os = "linux")]
mod memfd;

use crate::common::InvitationAddress;
use crate::protocol;
use futures::FutureExt;
use futures::StreamExt;
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
use std::io::IoSliceMut;
use std::os::unix::prelude::*;
use std::pin::pin;
use std::sync::Arc;
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
    send: async_channel::Sender<SenderCommand>,
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

        // If using SOCK_SEQPACKET, try to send the packet immediately. If this
        // fails (likely due to EAGAIN), send the packet to the asynchronous
        // task for deferred processing.
        //
        // When SOCK_STREAM is in use, this optimization cannot be tried,
        // because for stream sockets we may need to issue multiple writes to
        // send the whole message, and those writes cannot be interleaved
        // correctly.
        //
        // N.B. This can lead to out of order messages. The event protocol is
        //      responsible for handling this condition.
        if !USE_SEQPACKET
            || try_send(
                self.socket.socket.lock().get(),
                &[IoSlice::new(&packet)],
                &fds,
            )
            .is_err()
        {
            let _ = self.send.send_blocking(SenderCommand::Send { packet, fds });
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
            reserved: [0; 7],
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
        self.send.close();
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
    let (send, recv) = async_channel::unbounded();
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
    send_send: async_channel::Sender<SenderCommand>,
    send_recv: async_channel::Receiver<SenderCommand>,
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
    send: &async_channel::Sender<SenderCommand>,
) -> Result<(), ReceiveError> {
    let mut buf = vec![0; MAX_PACKET_SIZE];
    let mut fds = Vec::new();
    loop {
        let len = socket.recv(&mut buf, &mut fds).await?;
        if len == 0 {
            break;
        }
        if cfg!(target_os = "macos") && !fds.is_empty() {
            // Tell the opposite endpoint to release the fds it sent.
            let _ = send
                .send(SenderCommand::Send {
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
                })
                .await;
        }

        let buf = &buf[..len];
        let header = protocol::PacketHeader::read_from_prefix(buf)
            .map_err(|_| ReceiveError::NoHeader)?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
        match header.packet_type {
            protocol::PacketType::EVENT => {
                local_node.event(remote_id, &buf[size_of_val(&header)..], &mut fds);
                fds.clear();
            }
            protocol::PacketType::RELEASE_FDS => {
                let release_fds = protocol::ReleaseFds::read_from_prefix(buf)
                    .map_err(|_| ReceiveError::BadReleaseFds)?
                    .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                let _ = send
                    .send(SenderCommand::ReleaseFds {
                        count: release_fds.count as usize,
                    })
                    .await;
            }
            #[cfg(target_os = "linux")]
            protocol::PacketType::LARGE_EVENT => {
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

#[derive(Debug, Error)]
enum ProtocolError {
    #[error("request to release too many fds")]
    ReleasingTooManyFds,
}

/// Handles send processing for the socket.
async fn run_send(
    recv: async_channel::Receiver<SenderCommand>,
    socket: &UnixSocket,
    retained_fds: &mut VecDeque<OsResource>,
) -> io::Result<()> {
    while let Ok(command) = recv.recv().await {
        match command {
            SenderCommand::Send { packet, fds } => {
                match socket.send(&packet, &fds).await {
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
    pub async fn invite(&self, port: Port) -> io::Result<Invitation> {
        let (invitation_send, mut invitation_recv) = channel();
        self.to_leader
            .send(LeaderRequest::Invite(port, invitation_send));
        let invitation = invitation_recv
            .recv()
            .await
            .map_err(|_| ErrorKind::ConnectionReset)?;
        tracing::debug!(
            invite_id = ?invitation.address.local_addr.node,
            "received invitation",
        );
        Ok(invitation)
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

/// An AF_UNIX SOCK_SEQPACKET connection.
struct UnixSocket {
    socket: Mutex<PolledSocket<Socket>>,
}

#[repr(C)]
struct CmsgScmRights {
    hdr: libc::cmsghdr,
    fds: [RawFd; 64],
}

// TODO: replace this copy+paste of IoSlice::advance_slices with std's
// implementation once stabilized.
fn advance_slices(bufs: &mut &mut [IoSlice<'_>], n: usize) {
    // Number of buffers to remove.
    let mut remove = 0;
    // Total length of all the to be removed buffers.
    let mut accumulated_len = 0;
    for buf in bufs.iter() {
        if accumulated_len + buf.len() > n {
            break;
        } else {
            accumulated_len += buf.len();
            remove += 1;
        }
    }

    *bufs = &mut std::mem::take(bufs)[remove..];
    if !bufs.is_empty() {
        let buf = bufs[0];
        // SAFETY: this transmute extends the lifetime, which is necessary
        // because IoSlice<'a> does not have a method to get the inner slice
        // with lifetime 'a, even though this is perfectly safe and is necessary
        // to implement this function.
        bufs[0] = unsafe {
            std::mem::transmute::<IoSlice<'_>, IoSlice<'_>>(IoSlice::new(
                &buf[n - accumulated_len..],
            ))
        };
    }
}

impl UnixSocket {
    fn new(driver: &dyn SpawnDriver, fd: Socket) -> Self {
        let socket = PolledSocket::new(driver, fd).unwrap();
        UnixSocket {
            socket: Mutex::new(socket),
        }
    }

    async fn send(&self, msg: &[u8], fds: &[OsResource]) -> io::Result<()> {
        if USE_SEQPACKET {
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
        let n = poll_fn(|cx| {
            self.socket
                .lock()
                .poll_io(cx, InterestSlot::Write, PollEvents::OUT, |socket| {
                    try_send(socket.get(), iov, fds)
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
            advance_slices(&mut iov, n);
            fds = &[];
        }
        Ok(())
    }

    async fn recv(&self, buf: &mut [u8], fds: &mut Vec<OsResource>) -> io::Result<usize> {
        if USE_SEQPACKET {
            self.recv_raw(buf, fds).await
        } else {
            let mut len = [0; 4];
            if !self.recv_all_raw(&mut len, fds).await? {
                return Ok(0);
            }
            let len = u32::from_le_bytes(len) as usize;
            let buf = buf
                .get_mut(..len)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EMSGSIZE))?;
            if !self.recv_all_raw(buf, fds).await? {
                return Err(ErrorKind::UnexpectedEof.into());
            }
            Ok(len)
        }
    }

    async fn recv_all_raw(
        &self,
        buf: &mut [u8],
        fds: &mut Vec<OsResource>,
    ) -> Result<bool, io::Error> {
        let mut read = 0;
        while read < buf.len() {
            let n = self.recv_raw(&mut buf[read..], fds).await?;
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
    ) -> Result<usize, io::Error> {
        let n = poll_fn(|cx| {
            self.socket
                .lock()
                .poll_io(cx, InterestSlot::Read, PollEvents::IN, |socket| {
                    try_recv(socket.get(), buf, fds)
                })
        })
        .await?;
        Ok(n)
    }

    async fn close_write(&self) -> io::Result<()> {
        self.socket.lock().get().shutdown(std::net::Shutdown::Write)
    }
}

/// Sends a packet, including the specified file descriptors. May fail with
/// ErrorKind::WouldBlock.
// x86_64-unknown-linux-musl targets have a different type defn for
// `libc::cmsghdr`, hence why these lints are being suppressed.
#[allow(clippy::needless_update, clippy::useless_conversion)]
fn try_send(socket: &Socket, msg: &[IoSlice<'_>], fds: &[OsResource]) -> io::Result<usize> {
    let mut cmsg = CmsgScmRights {
        hdr: libc::cmsghdr {
            cmsg_level: libc::SOL_SOCKET,
            cmsg_type: libc::SCM_RIGHTS,
            cmsg_len: (size_of::<libc::cmsghdr>() + size_of_val(fds))
                .try_into()
                .unwrap(),

            ..{
                // SAFETY: type has no invariants
                unsafe { std::mem::zeroed() }
            }
        },
        fds: [0; 64],
    };
    for (fdi, fdo) in fds.iter().zip(cmsg.fds.iter_mut()) {
        *fdo = match fdi {
            OsResource::Fd(fd) => fd.as_raw_fd(),
        }
    }

    // SAFETY: type has no invariants
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = msg.as_ptr() as *mut libc::iovec;
    hdr.msg_iovlen = msg.len().try_into().unwrap();
    hdr.msg_control = if fds.is_empty() {
        std::ptr::null_mut()
    } else {
        std::ptr::from_mut(&mut cmsg).cast::<libc::c_void>()
    };
    hdr.msg_controllen = if fds.is_empty() { 0 } else { cmsg.hdr.cmsg_len };
    // SAFETY: calling with appropriately initialized buffers.
    let n = unsafe { libc::sendmsg(socket.as_raw_fd(), &hdr, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Receives the next packet. Returns the number of bytes read and any file
/// descriptors that were associated with the packet. May fail with
/// ErrorKind::WouldBlock.
fn try_recv(socket: &Socket, buf: &mut [u8], fds: &mut Vec<OsResource>) -> io::Result<usize> {
    assert!(!buf.is_empty());
    let mut iov = IoSliceMut::new(buf);
    // SAFETY: type has no invariants
    let mut cmsg: CmsgScmRights = unsafe { std::mem::zeroed() };
    // SAFETY: type has no invariants
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = std::ptr::from_mut(&mut iov).cast::<libc::iovec>();
    hdr.msg_iovlen = 1;
    hdr.msg_control = std::ptr::from_mut(&mut cmsg).cast::<libc::c_void>();
    hdr.msg_controllen = size_of_val(&cmsg) as _;

    // On Linux, automatically set O_CLOEXEC on incoming fds.
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;

    // SAFETY: calling with properly initialized buffers.
    let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut hdr, flags) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        assert_eq!(hdr.msg_controllen, 0);
        return Ok(0);
    }

    let fd_count = if hdr.msg_controllen > 0 {
        if cmsg.hdr.cmsg_level != libc::SOL_SOCKET || cmsg.hdr.cmsg_type != libc::SCM_RIGHTS {
            // BUGBUG: need to loop: possible to leak fds
            return Err(ErrorKind::InvalidData.into());
        }
        #[allow(clippy::unnecessary_cast)] // cmsg_len is u32 on musl and usize on gnu.
        {
            (cmsg.hdr.cmsg_len as usize - size_of_val(&cmsg.hdr)) / size_of::<RawFd>()
        }
    } else {
        0
    };

    let start = fds.len();
    fds.extend(cmsg.fds[..fd_count].iter().map(|x| {
        // SAFETY: according to the contract with the kernel, this
        // fd is now owned by the process.
        OsResource::Fd(unsafe { OwnedFd::from_raw_fd(*x) })
    }));

    // Set O_CLOEXEC on all received fds on platforms that don't support
    // MSG_CMSG_CLOEXEC (set above).
    if !cfg!(target_os = "linux") {
        for OsResource::Fd(fd) in &fds[start..] {
            set_cloexec(fd);
        }
    }

    // Check for truncation only after taking ownership of the fds.
    if hdr.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(io::Error::from_raw_os_error(libc::EMSGSIZE));
    }
    Ok(n as usize)
}

fn set_cloexec(fd: impl AsFd) {
    // SAFETY: using fcntl as documented.
    unsafe {
        let flags = libc::fcntl(fd.as_fd().as_raw_fd(), libc::F_GETFD);
        assert!(flags >= 0);
        let r = libc::fcntl(
            fd.as_fd().as_raw_fd(),
            libc::F_SETFD,
            flags | libc::FD_CLOEXEC,
        );
        assert!(r >= 0);
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
}
