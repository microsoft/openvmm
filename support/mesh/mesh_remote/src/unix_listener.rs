// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh listener for accepting mesh connections over Unix sockets.
//!
//! This module composes the payload transport ([`crate::unix_payload`]) with the
//! mesh inviter ([`crate::unix_node::UnixMeshInviter`]) to provide a
//! caller-facing API for mesh servers and clients.

#![cfg(unix)]

use crate::unix_node::Invitation;
use crate::unix_node::InviteError;
use crate::unix_node::JoinError;
use crate::unix_node::UnixMeshInviter;
use crate::unix_node::UnixNode;
use crate::unix_payload::RecvPayloadError;
use crate::unix_payload::SendPayloadError;
use crate::unix_payload::UnixPayloadListener;
use crate::unix_payload::recv_payload;
use crate::unix_payload::send_payload;
use crate::unix_payload::unix_payload_connect;
use mesh_node::local_node::Port;
use pal_async::driver::Driver;
use pal_async::driver::SpawnDriver;
use pal_async::socket::PolledSocket;
use std::io;
use std::path::Path;
use thiserror::Error;
use unix_socket::UnixStream;

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
    listener: UnixPayloadListener,
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
    /// The caller is responsible for removing any existing socket file at
    /// `path` before calling this.
    fn bind(
        driver: &(impl Driver + ?Sized),
        inviter: UnixMeshInviter,
        path: &Path,
    ) -> Result<Self, ListenError> {
        let listener = UnixPayloadListener::bind(driver, path).map_err(ListenError)?;
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
        let stream = self.listener.accept(driver).await.map_err(AcceptError)?;
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
        let mut stream = unix_payload_connect(&driver, path)
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
