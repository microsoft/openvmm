// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh listener for accepting mesh connections over Unix sockets.
//!
//! Provides a framed [`MeshPayload`]-over-Unix-stream transport (with
//! SCM_RIGHTS fd passing via [`crate::unix_common`]) and composes it with the
//! mesh inviter ([`crate::unix_node::UnixMeshInviter`]) to let external
//! processes join a running mesh.

#![cfg(unix)]

use crate::unix_common::try_recv;
use crate::unix_common::try_send;
use crate::unix_node::Invitation;
use crate::unix_node::InviteError;
use crate::unix_node::JoinError;
use crate::unix_node::UnixMeshInviter;
use crate::unix_node::UnixNode;
use mesh_node::local_node::Port;
use mesh_node::message::MeshPayload;
use mesh_node::resource::OsResource;
use mesh_node::resource::Resource;
use mesh_protobuf::SerializedMessage;
use pal_async::driver::Driver;
use pal_async::driver::SpawnDriver;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::PolledSocket;
use std::future::poll_fn;
use std::io;
use std::io::IoSlice;
use std::os::unix::prelude::*;
use std::path::Path;
use thiserror::Error;
use unix_socket::UnixListener;
use unix_socket::UnixStream;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// ---------------------------------------------------------------------------
// Payload framing — wire format + send/recv over a Unix stream
// ---------------------------------------------------------------------------

/// Wire format header for payload messages.
///
/// Followed by `data_len` bytes of protobuf data, plus SCM_RIGHTS ancillary
/// data carrying `fd_count` file descriptors.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct PayloadHeader {
    /// Length of the protobuf data that follows the header, in bytes (LE).
    data_len: u32,
    /// Number of file descriptors carried via SCM_RIGHTS (LE).
    fd_count: u32,
}

/// Error returned by [`send_payload`].
#[derive(Debug, Error)]
pub enum SendPayloadError {
    /// An I/O error occurred while sending.
    #[error("failed to send payload")]
    Io(#[source] io::Error),
}

/// Error returned by [`recv_payload`].
#[derive(Debug, Error)]
pub enum RecvPayloadError {
    /// The payload data length exceeded the maximum allowed size.
    #[error("payload data too large ({len} bytes, max {max})")]
    DataTooLarge { len: usize, max: usize },
    /// The header declared more file descriptors than allowed.
    #[error("too many file descriptors in payload header ({count}, max {max})")]
    TooManyFds { count: usize, max: usize },
    /// The number of file descriptors received did not match the header.
    #[error("expected {expected} file descriptors, received {actual}")]
    FdCountMismatch { expected: usize, actual: usize },
    /// The received payload could not be deserialized into the expected type.
    #[error("failed to deserialize payload")]
    Deserialize(#[source] mesh_protobuf::Error),
    /// An I/O error occurred while receiving.
    #[error("failed to receive payload")]
    Io(#[source] io::Error),
}

/// Send a [`MeshPayload`] value over a Unix stream, transferring OS resources
/// via SCM_RIGHTS.
async fn send_payload<T: MeshPayload>(
    stream: &mut PolledSocket<UnixStream>,
    value: T,
) -> Result<(), SendPayloadError> {
    let msg: SerializedMessage<Resource> = SerializedMessage::from_message(value);
    send_serialized(stream, msg).await
}

/// Non-generic inner function for [`send_payload`].
async fn send_serialized(
    stream: &mut PolledSocket<UnixStream>,
    msg: SerializedMessage<Resource>,
) -> Result<(), SendPayloadError> {
    let mut fds = Vec::new();
    for resource in msg.resources {
        match resource {
            Resource::Os(os) => fds.push(os),
            Resource::Port(_) => {
                unreachable!("send_payload is only used with types that have OS resources")
            }
        }
    }

    let header = PayloadHeader {
        data_len: msg.data.len() as u32,
        fd_count: fds.len() as u32,
    };
    let header_bytes = header.as_bytes();

    // Send the entire message (header + data + fds) in a single sendmsg call.
    // For stream sockets, we may need to retry on partial writes.
    let total_len = header_bytes.len() + msg.data.len();
    let mut sent = 0usize;
    let mut fds_sent = false;
    while sent < total_len {
        let n = poll_fn(|cx| {
            stream.poll_io(cx, InterestSlot::Write, PollEvents::OUT, |stream| {
                // Build iov slices for the remaining data.
                let header_remaining = if sent < size_of::<PayloadHeader>() {
                    &header_bytes[sent..]
                } else {
                    &[]
                };
                let data_offset = sent.saturating_sub(size_of::<PayloadHeader>());
                let data_remaining = &msg.data[data_offset..];
                let bufs = [IoSlice::new(header_remaining), IoSlice::new(data_remaining)];
                let send_fds = if fds_sent { &[] } else { &fds[..] };
                try_send(stream.get().as_fd(), &bufs, send_fds)
            })
        })
        .await
        .map_err(SendPayloadError::Io)?;
        if !fds_sent {
            fds_sent = true;
        }
        sent += n;
    }

    Ok(())
}

/// Receive a [`MeshPayload`] value from a Unix stream, receiving OS resources
/// via SCM_RIGHTS.
async fn recv_payload<T: MeshPayload>(
    stream: &mut PolledSocket<UnixStream>,
) -> Result<T, RecvPayloadError> {
    let msg = recv_serialized(stream).await?;
    msg.into_message().map_err(RecvPayloadError::Deserialize)
}

/// Non-generic inner function for [`recv_payload`].
async fn recv_serialized(
    stream: &mut PolledSocket<UnixStream>,
) -> Result<SerializedMessage<Resource>, RecvPayloadError> {
    // Fds may arrive with any recvmsg call — collect them across all reads.
    let mut fds = Vec::new();

    // Read the header.
    let mut header = PayloadHeader::new_zeroed();
    recv_exact_with_fds(stream, header.as_mut_bytes(), &mut fds)
        .await
        .map_err(RecvPayloadError::Io)?;

    let data_len = header.data_len as usize;
    let fd_count = header.fd_count as usize;

    // Validate sizes to prevent DoS from a malicious peer.
    const MAX_DATA_LEN: usize = 4096;
    const MAX_FD_COUNT: usize = 4;
    if data_len > MAX_DATA_LEN {
        return Err(RecvPayloadError::DataTooLarge {
            len: data_len,
            max: MAX_DATA_LEN,
        });
    }
    if fd_count > MAX_FD_COUNT {
        return Err(RecvPayloadError::TooManyFds {
            count: fd_count,
            max: MAX_FD_COUNT,
        });
    }

    // Read the data (fds may also arrive here if the header read was split).
    let mut data = vec![0u8; data_len];
    if !data.is_empty() {
        recv_exact_with_fds(stream, &mut data, &mut fds)
            .await
            .map_err(RecvPayloadError::Io)?;
    }

    if fds.len() != fd_count {
        return Err(RecvPayloadError::FdCountMismatch {
            expected: fd_count,
            actual: fds.len(),
        });
    }

    // Convert OsResource fds back to Resource.
    let resources: Vec<Resource> = fds.into_iter().map(Resource::Os).collect();

    Ok(SerializedMessage { data, resources })
}

/// Read exactly `buf.len()` bytes from the stream, collecting any fds received.
async fn recv_exact_with_fds(
    stream: &mut PolledSocket<UnixStream>,
    buf: &mut [u8],
    fds: &mut Vec<OsResource>,
) -> io::Result<()> {
    let mut read = 0;
    while read < buf.len() {
        let n = poll_fn(|cx| {
            stream.poll_io(cx, InterestSlot::Read, PollEvents::IN, |stream| {
                try_recv(stream.get().as_fd(), &mut buf[read..], fds)
            })
        })
        .await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        read += n;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Mesh listener — listen/accept/handshake + join_by_path
// ---------------------------------------------------------------------------

/// Error returned by [`UnixNode::listen`].
#[derive(Debug, Error)]
#[error("failed to bind listener socket")]
pub struct ListenError(#[source] pub io::Error);

/// Error returned by [`UnixMeshListener::accept`].
#[derive(Debug, Error)]
#[error("failed to accept connection")]
pub struct AcceptError(#[source] pub io::Error);

/// Error returned by [`PendingMeshConnection::finish`].
#[derive(Debug, Error)]
pub enum HandshakeError {
    /// Creating the mesh invitation failed (node shut down).
    #[error("failed to create mesh invitation")]
    Invite(#[source] InviteError),
    /// Sending the invitation to the client failed.
    #[error("failed to send invitation to client")]
    Send(#[source] SendPayloadError),
}

/// Error returned by [`UnixNode::join_by_path`].
#[derive(Debug, Error)]
pub enum JoinByPathError {
    /// Failed to connect to the listener socket.
    #[error("failed to connect to mesh listener")]
    Connect(#[source] io::Error),
    /// Failed to receive the invitation from the listener.
    #[error("failed to receive invitation")]
    Recv(#[source] RecvPayloadError),
    /// Failed to join the mesh with the received invitation.
    #[error("failed to join mesh")]
    Join(#[source] JoinError),
}

/// A listener that accepts mesh connections over a Unix socket.
///
/// The listener binds to a Unix socket path and hands out mesh invitations to
/// connecting clients, allowing them to join the mesh.
pub struct UnixMeshListener {
    listener: PolledSocket<UnixListener>,
    inviter: UnixMeshInviter,
}

/// A pending mesh connection that has been accepted but not yet handshaked.
///
/// Call [`finish`](PendingMeshConnection::finish) to complete the handshake
/// (create invitation, send it to the client).
///
/// This type exists so that the accept loop is never blocked by a slow or
/// malicious client. The caller should spawn `finish()` as a separate task.
pub struct PendingMeshConnection {
    stream: PolledSocket<UnixStream>,
    inviter: UnixMeshInviter,
}

impl UnixMeshListener {
    /// Bind to a Unix socket path.
    ///
    /// # Security
    ///
    /// The socket inherits the process's umask permissions. Any local user
    /// who can connect to the socket will be able to join the mesh. The
    /// caller must ensure `path` is inside a directory that is only
    /// accessible to the intended user (e.g. a `0700` directory such as
    /// `$XDG_RUNTIME_DIR`). Directory-level permissions are the only
    /// reliable protection — `fchmod` after `bind` leaves a race window,
    /// and `SO_PEERCRED` checks can be bypassed via symlink attacks in a
    /// shared directory.
    ///
    /// The caller is responsible for removing any existing socket file at
    /// `path` before calling this.
    fn bind(
        driver: &(impl Driver + ?Sized),
        inviter: UnixMeshInviter,
        path: &Path,
    ) -> Result<Self, ListenError> {
        let listener = UnixListener::bind(path).map_err(ListenError)?;
        let listener = PolledSocket::new(driver, listener).map_err(ListenError)?;
        Ok(Self { listener, inviter })
    }

    /// Accept a new connection on the listener socket.
    ///
    /// Returns a [`PendingMeshConnection`] immediately after the socket-level
    /// accept. The handshake (invitation creation + send) has NOT happened
    /// yet — call `pending.finish()` to complete it.
    ///
    /// Typical usage: spawn `finish()` as a separate task so the accept loop
    /// is never blocked by a slow client.
    pub async fn accept(
        &mut self,
        driver: &(impl Driver + ?Sized),
    ) -> Result<PendingMeshConnection, AcceptError> {
        let (stream, _addr) = self.listener.accept().await.map_err(AcceptError)?;
        let stream = PolledSocket::new(driver, stream).map_err(AcceptError)?;
        Ok(PendingMeshConnection {
            stream,
            inviter: self.inviter.clone(),
        })
    }
}

impl PendingMeshConnection {
    /// Complete the handshake: create a mesh invitation and send it to the
    /// connecting client.
    ///
    /// The handshake stream is dropped after sending — mesh communication
    /// happens on the socketpair inside the invitation.
    ///
    /// This may block if the client is slow to read. Callers should spawn this
    /// as a separate task rather than awaiting it inline in the accept loop.
    pub async fn finish(mut self, port: Port) -> Result<(), HandshakeError> {
        let invitation = self
            .inviter
            .invite(port)
            .await
            .map_err(HandshakeError::Invite)?;
        send_payload(&mut self.stream, invitation)
            .await
            .map_err(HandshakeError::Send)
    }
}

impl UnixNode {
    /// Listen for mesh connections on a Unix socket path.
    ///
    /// Creates a [`UnixMeshListener`] bound to `path` that will accept mesh
    /// connections on behalf of this node.
    ///
    /// # Security
    ///
    /// `path` must be inside a directory accessible only to the intended
    /// user (e.g. `$XDG_RUNTIME_DIR`, mode `0700`). See `UnixMeshListener::bind`
    /// for details.
    ///
    /// The caller is responsible for removing any existing socket file at
    /// `path` before calling this.
    pub fn listen(
        &self,
        driver: &(impl Driver + ?Sized),
        path: &Path,
    ) -> Result<UnixMeshListener, ListenError> {
        UnixMeshListener::bind(driver, self.inviter(), path)
    }

    /// Connect to a mesh listener at `path` and join the mesh.
    ///
    /// Connects to a [`UnixMeshListener`], receives an invitation, and joins
    /// the mesh, returning a new [`UnixNode`] bridged to `port`.
    pub async fn join_by_path(
        driver: impl SpawnDriver,
        path: &Path,
        port: Port,
    ) -> Result<Self, JoinByPathError> {
        let mut stream = PolledSocket::connect_unix(&driver, path)
            .await
            .map_err(JoinByPathError::Connect)?;
        let invitation: Invitation = recv_payload(&mut stream)
            .await
            .map_err(JoinByPathError::Recv)?;
        drop(stream);
        Self::join(driver, invitation, port)
            .await
            .map_err(JoinByPathError::Join)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use test_with_tracing::test;

    #[derive(Debug, PartialEq, mesh_protobuf::Protobuf)]
    struct SimplePayload {
        value: u64,
        text: String,
    }

    #[async_test]
    async fn test_send_recv_simple_payload(driver: DefaultDriver) {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("test.sock");

        let listener = UnixListener::bind(&sock_path).unwrap();
        let mut listener = PolledSocket::new(&driver, listener).unwrap();

        let connect_driver = driver.clone();
        let connect_path = sock_path.clone();
        let client_task = driver.spawn("client", async move {
            let mut stream = PolledSocket::connect_unix(&connect_driver, &connect_path)
                .await
                .unwrap();
            send_payload(
                &mut stream,
                SimplePayload {
                    value: 42,
                    text: "hello mesh".to_string(),
                },
            )
            .await
            .unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let stream = &mut PolledSocket::new(&driver, stream).unwrap();
        let received: SimplePayload = recv_payload(stream).await.unwrap();
        assert_eq!(received.value, 42);
        assert_eq!(received.text, "hello mesh");

        client_task.await;
    }

    #[derive(Debug, PartialEq, mesh_protobuf::Protobuf)]
    struct TestMessage {
        value: u64,
        text: String,
    }

    #[async_test]
    async fn test_end_to_end(driver: DefaultDriver) {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("mesh.sock");

        // Create the leader node and listen for connections.
        let leader = UnixNode::new(driver.clone());
        let mut listener = leader.listen(&driver, &sock_path).unwrap();

        // Spawn a client task.
        let client_driver = driver.clone();
        let client_path = sock_path.clone();
        let client_task = driver.spawn("client", async move {
            let (sender, recv) = mesh_channel::channel::<TestMessage>();
            let node = UnixNode::join_by_path(client_driver, &client_path, recv.into())
                .await
                .unwrap();
            sender.send(TestMessage {
                value: 12345,
                text: "hello from client".to_string(),
            });
            // Keep the node alive until the message is delivered.
            node.shutdown().await;
        });

        // Server accepts and finishes the handshake.
        let pending = listener.accept(&driver).await.unwrap();
        let (send, mut recv) = mesh_channel::channel::<TestMessage>();
        pending.finish(send.into()).await.unwrap();

        // Receive the message.
        let msg = recv.recv().await.unwrap();
        assert_eq!(msg.value, 12345);
        assert_eq!(msg.text, "hello from client");

        client_task.await;
        leader.shutdown().await;
    }
}
