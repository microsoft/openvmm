// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix socket listener for distributing ALPC mesh invitations.
//!
//! This module provides [`AlpcMeshListener`] for servers and
//! [`AlpcNode::join_by_socket`] for clients. The Unix socket is used only for
//! the invitation handshake — mesh communication happens over ALPC.
//!
//! Security is enforced by the filesystem: the socket file inherits the
//! permissions of its parent directory, so placing it in a user-owned directory
//! prevents other users from connecting.

#![cfg(windows)]

use crate::alpc_node::AlpcMeshInviter;
use crate::alpc_node::AlpcNode;
use crate::alpc_node::InviteError;
use crate::alpc_node::JoinError;
use crate::alpc_node::NamedInvitation;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use mesh_node::local_node::Port;
use pal_async::driver::Driver;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use unix_socket::UnixListener;
use unix_socket::UnixStream;

/// A listener that accepts mesh connections over a Unix socket.
///
/// The server listens on the given socket path. When a client connects, the
/// server creates a mesh invitation and sends it over the socket. The client
/// deserializes the invitation and calls [`AlpcNode::join_named()`].
///
/// The socket file is removed when the listener is dropped.
pub struct AlpcMeshListener {
    listener: PolledSocket<UnixListener>,
    inviter: AlpcMeshInviter,
    path: PathBuf,
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
    inviter: AlpcMeshInviter,
}

impl AlpcMeshListener {
    /// Create a Unix socket listener.
    ///
    /// `path` is the filesystem path for the socket (e.g.,
    /// `C:\Users\<user>\AppData\Local\openvmm\<vm-name>.sock`).
    ///
    /// Any existing socket file at `path` is removed before binding.
    fn create(
        driver: &(impl Driver + ?Sized),
        inviter: AlpcMeshInviter,
        path: &Path,
    ) -> io::Result<Self> {
        // Remove stale socket file if it exists, but only if it's actually
        // a Unix socket — avoid deleting an unrelated file at the same path.
        if pal::windows::fs::is_unix_socket(path).unwrap_or(false) {
            let _ = std::fs::remove_file(path);
        }
        let listener = UnixListener::bind(path)?;
        let listener = PolledSocket::new(driver, listener)?;
        Ok(Self {
            listener,
            inviter,
            path: path.to_owned(),
        })
    }

    /// Accept a new connection on the Unix socket.
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
    ) -> io::Result<PendingMeshConnection> {
        let (stream, _addr) = self.listener.accept().await?;
        let stream = PolledSocket::new(driver, stream)?;
        Ok(PendingMeshConnection {
            stream,
            inviter: self.inviter.clone(),
        })
    }
}

impl Drop for AlpcMeshListener {
    fn drop(&mut self) {
        if pal::windows::fs::is_unix_socket(&self.path).unwrap_or(false) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl PendingMeshConnection {
    /// Complete the handshake: create a mesh invitation and send it to the
    /// connecting client.
    ///
    /// The handshake socket is dropped after sending — mesh communication
    /// happens over ALPC.
    ///
    /// This may block if the client is slow to read. Callers should spawn this
    /// as a separate task rather than awaiting it inline in the accept loop.
    pub async fn finish(mut self, port: Port) -> Result<(), FinishError> {
        let (invitation, handle) = self
            .inviter
            .invite_named(port)
            .await
            .map_err(FinishError::Invite)?;

        let data = mesh_protobuf::encode(invitation);

        let len = data.len() as u32;
        self.stream.write_all(&len.to_le_bytes()).await?;
        self.stream.write_all(&data).await?;
        self.stream.flush().await?;

        // Wait for the client to join the mesh via the invitation.
        handle.await;
        Ok(())
    }
}

impl AlpcNode {
    /// Listen for mesh connections on a Unix socket.
    ///
    /// This is a convenience method that extracts an inviter and creates an
    /// [`AlpcMeshListener`] in one step.
    ///
    /// It is primarily intended for nodes created with [`AlpcNode::new_named`],
    /// since the listener hands out named invitations during the handshake.
    /// If used with other kinds of nodes, pending connections may later fail
    /// during the handshake phase when a named invitation cannot be created.
    pub fn listen(
        &self,
        driver: &(impl Driver + ?Sized),
        path: &Path,
    ) -> io::Result<AlpcMeshListener> {
        AlpcMeshListener::create(driver, self.inviter(), path)
    }

    /// Connect to a mesh listener at `path` and join the mesh.
    ///
    /// This is a convenience method that connects to an [`AlpcMeshListener`],
    /// receives an invitation, and joins the mesh in one step.
    pub async fn join_by_socket(
        driver: impl Driver + Spawn + Clone,
        path: &Path,
        port: Port,
    ) -> Result<Self, JoinBySocketError> {
        let mut stream = PolledSocket::<UnixStream>::connect_unix(&driver, path)
            .await
            .map_err(JoinBySocketError::Connect)?;

        // Read the length-prefixed invitation: [4 bytes LE length][data].
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .await
            .map_err(JoinBySocketError::Read)?;
        let data_len = u32::from_le_bytes(len_buf) as usize;

        const MAX_INVITATION_SIZE: usize = 64 * 1024;
        if data_len > MAX_INVITATION_SIZE {
            return Err(JoinBySocketError::InvitationTooLarge { len: data_len });
        }

        let mut data = vec![0u8; data_len];
        stream
            .read_exact(&mut data)
            .await
            .map_err(JoinBySocketError::Read)?;
        drop(stream);

        let invitation: NamedInvitation =
            mesh_protobuf::decode(&data).map_err(JoinBySocketError::Decode)?;

        AlpcNode::join_named(driver, invitation, port).map_err(JoinBySocketError::Join)
    }
}

/// Errors from [`AlpcNode::join_by_socket`].
#[derive(Debug, thiserror::Error)]
#[expect(missing_docs)]
pub enum JoinBySocketError {
    #[error("failed to connect to mesh socket")]
    Connect(#[source] io::Error),
    #[error("failed to read invitation from mesh socket")]
    Read(#[source] io::Error),
    #[error("invitation too large ({len} bytes)")]
    InvitationTooLarge { len: usize },
    #[error("failed to decode invitation")]
    Decode(#[source] mesh_protobuf::Error),
    #[error("failed to join mesh")]
    Join(#[source] JoinError),
}

/// Errors from [`PendingMeshConnection::finish`].
#[derive(Debug, thiserror::Error)]
#[expect(missing_docs)]
pub enum FinishError {
    #[error("failed to create invitation")]
    Invite(#[source] InviteError),
    #[error("failed to send invitation over socket")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_protobuf::Protobuf;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;

    #[derive(Debug, PartialEq, Protobuf)]
    struct TestMessage {
        value: u32,
    }

    #[async_test]
    async fn test_unix_socket_end_to_end(driver: DefaultDriver) {
        let mut name_bytes = [0u8; 16];
        getrandom::fill(&mut name_bytes).unwrap();
        let socket_name = format!("mesh-test-{:0x}.sock", u128::from_ne_bytes(name_bytes));
        let socket_path = std::env::temp_dir().join(&socket_name);

        // Create the leader node and listen for connections.
        let leader = AlpcNode::new_named(driver.clone()).unwrap();
        let mut listener = leader.listen(&driver, &socket_path).unwrap();

        // Use join! to drive accept and connect concurrently.
        let client_driver = driver.clone();
        let client_socket_path = socket_path.clone();

        let (mut recv, client_node) = futures::join!(
            async {
                let pending = listener.accept(&driver).await.unwrap();
                let (send, recv) = mesh_channel::channel::<TestMessage>();
                pending.finish(send.into()).await.unwrap();
                recv
            },
            async {
                let (send, recv) = mesh_channel::channel::<TestMessage>();
                let node =
                    AlpcNode::join_by_socket(client_driver, &client_socket_path, recv.into())
                        .await
                        .unwrap();
                send.send(TestMessage { value: 12345 });
                node
            }
        );

        let msg = recv.recv().await.unwrap();
        assert_eq!(msg.value, 12345);

        drop(recv);
        client_node.shutdown().await;
        leader.shutdown().await;
    }
}
