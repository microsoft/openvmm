// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Named pipe listener for distributing ALPC mesh invitations.
//!
//! This module provides [`AlpcMeshListener`] for servers and
//! [`AlpcNode::join_by_pipe`] for clients. The named pipe is used only for the
//! invitation handshake — mesh communication happens over ALPC.

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
use pal_async::pipe::PolledPipe;
use pal_async::task::Spawn;
use pal_async::windows::pipe::NamedPipeServer;
use std::fs::OpenOptions;
use std::io;

/// A listener that accepts mesh connections over a named pipe.
///
/// The server listens on `\\.\pipe\<pipe_name>`. When a client connects, the
/// server creates a mesh invitation and sends it over the pipe. The client
/// deserializes the invitation and calls [`AlpcNode::join_named()`].
pub struct AlpcMeshListener {
    server: NamedPipeServer,
    inviter: AlpcMeshInviter,
}

/// A pending mesh connection that has been accepted but not yet handshaked.
///
/// Call [`finish`](PendingMeshConnection::finish) to complete the handshake
/// (create invitation, send it to the client).
///
/// This type exists so that the accept loop is never blocked by a slow or
/// malicious client. The caller should spawn `finish()` as a separate task.
pub struct PendingMeshConnection {
    pipe: PolledPipe,
    inviter: AlpcMeshInviter,
}

impl AlpcMeshListener {
    /// Create a named pipe listener.
    ///
    /// `pipe_name` is the pipe name (e.g., `openvmm-<vm-name>`).
    /// The full path will be `\\.\pipe\<pipe_name>`.
    fn create(inviter: AlpcMeshInviter, pipe_name: &str) -> io::Result<Self> {
        let path = format!(r"\\.\pipe\{pipe_name}");
        let server = NamedPipeServer::create(&path)?;
        Ok(Self { server, inviter })
    }

    /// Accept a new connection on the named pipe.
    ///
    /// Returns a [`PendingMeshConnection`] immediately after the pipe-level
    /// accept. The handshake (invitation creation + send) has NOT happened
    /// yet — call `pending.finish()` to complete it.
    ///
    /// Typical usage: spawn `finish()` as a separate task so the accept loop
    /// is never blocked by a slow client.
    pub async fn accept(
        &mut self,
        driver: &(impl Driver + ?Sized),
    ) -> io::Result<PendingMeshConnection> {
        let listening = self.server.accept(driver)?;
        let file = listening.await?;
        let pipe = PolledPipe::new(driver, file)?;
        Ok(PendingMeshConnection {
            pipe,
            inviter: self.inviter.clone(),
        })
    }
}

impl PendingMeshConnection {
    /// Complete the handshake: create a mesh invitation and send it to the
    /// connecting client.
    ///
    /// The handshake pipe is dropped after sending — mesh communication
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
        self.pipe.write_all(&len.to_le_bytes()).await?;
        self.pipe.write_all(&data).await?;
        self.pipe.flush().await?;

        // Wait for the client to join the mesh via the invitation.
        handle.await;
        Ok(())
    }
}

impl AlpcNode {
    /// Listen for mesh connections on a named pipe.
    ///
    /// This is a convenience method that extracts an inviter and creates an
    /// [`AlpcMeshListener`] in one step. Only works for named-directory nodes
    /// (created with [`AlpcNode::new_named`]).
    pub fn listen(&self, pipe_name: &str) -> io::Result<AlpcMeshListener> {
        AlpcMeshListener::create(self.inviter(), pipe_name)
    }

    /// Connect to a mesh listener at `pipe_name` and join the mesh.
    ///
    /// This is a convenience method that connects to an [`AlpcMeshListener`],
    /// receives an invitation, and joins the mesh in one step.
    pub async fn join_by_pipe(
        driver: impl Driver + Spawn + Clone,
        pipe_name: &str,
        port: Port,
    ) -> Result<Self, JoinByPipeError> {
        let path = format!(r"\\.\pipe\{pipe_name}");

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(JoinByPipeError::Connect)?;

        let mut pipe = PolledPipe::new(&driver, file).map_err(JoinByPipeError::Connect)?;

        // Read the framed invitation: [4 bytes LE: data_len][data_len bytes]
        let mut len_buf = [0u8; 4];
        pipe.read_exact(&mut len_buf)
            .await
            .map_err(JoinByPipeError::Connect)?;
        let data_len = u32::from_le_bytes(len_buf) as usize;

        const MAX_INVITATION_SIZE: usize = 64 * 1024;
        if data_len > MAX_INVITATION_SIZE {
            return Err(JoinByPipeError::InvitationTooLarge { len: data_len });
        }

        let mut data = vec![0u8; data_len];
        pipe.read_exact(&mut data)
            .await
            .map_err(JoinByPipeError::Connect)?;
        drop(pipe);

        let invitation: NamedInvitation =
            mesh_protobuf::decode(&data).map_err(JoinByPipeError::Decode)?;

        AlpcNode::join_named(driver, invitation, Vec::new(), port).map_err(JoinByPipeError::Join)
    }
}

/// Errors from [`AlpcNode::join_by_pipe`].
#[derive(Debug, thiserror::Error)]
#[expect(missing_docs)]
pub enum JoinByPipeError {
    #[error("failed to connect to mesh pipe")]
    Connect(#[source] io::Error),
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
    #[error("failed to send invitation over pipe")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_protobuf::Protobuf;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use test_with_tracing::test;

    #[derive(Debug, PartialEq, Protobuf)]
    struct TestMessage {
        value: u32,
    }

    #[async_test]
    async fn test_named_pipe_end_to_end(driver: DefaultDriver) {
        let mut name_bytes = [0u8; 16];
        getrandom::fill(&mut name_bytes).unwrap();
        let pipe_name = format!("mesh-test-{:0x}", u128::from_ne_bytes(name_bytes));

        // Create the leader node and listen for connections.
        let leader = AlpcNode::new_named(driver.clone()).unwrap();
        let mut listener = leader.listen(&pipe_name).unwrap();

        // Use join! to drive accept and connect concurrently.
        let client_driver = driver.clone();
        let client_pipe_name = pipe_name.clone();

        let (mut recv, client_node) = futures::join!(
            async {
                let pending = listener.accept(&driver).await.unwrap();
                let (send, recv) = mesh_channel::channel::<TestMessage>();
                pending.finish(send.into()).await.unwrap();
                recv
            },
            async {
                let (send, recv) = mesh_channel::channel::<TestMessage>();
                let node = AlpcNode::join_by_pipe(client_driver, &client_pipe_name, recv.into())
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
