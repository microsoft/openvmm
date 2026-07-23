// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host-side wrapper for the `RelayUnixSocket` pipette request.
//!
//! See [`pipette_protocol::RelayUnixSocketRequest`] for the protocol-level
//! contract. The wrapper here issues the request to a running pipette
//! agent and returns a [`PipeDuplex`] that wraps the host-retained halves
//! of the two mesh pipes used to pump bytes through the in-guest UNIX
//! listener.
//!
//! This is the primitive used to reach an L2 pipette in nested-virt tests:
//! the host hands the resulting `PipeDuplex` to [`PipetteClient::new`] just
//! like any other byte stream.
//!
//! [`PipetteClient::new`]: crate::PipetteClient::new

use crate::PipetteClient;
use anyhow::Context;
use futures::AsyncRead;
use futures::AsyncWrite;
use mesh::pipe::ReadPipe;
use mesh::pipe::WritePipe;
use pipette_protocol::PipetteRequest;
use pipette_protocol::RelayConnectUnixSocketRequest;
use pipette_protocol::RelayUnixSocketRequest;
use std::pin::Pin;
use std::task::Context as TaskContext;
use std::task::Poll;

/// A duplex byte stream backed by a pair of mesh pipes.
///
/// Produced by [`PipetteClient::relay_unix_socket`]. Reads pull bytes that
/// pipette has forwarded from the accepted UNIX-socket peer; writes push
/// bytes that pipette forwards to the peer.
pub struct PipeDuplex {
    read: ReadPipe,
    write: WritePipe,
}

impl AsyncRead for PipeDuplex {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().read).poll_read(cx, buf)
    }
}

impl AsyncWrite for PipeDuplex {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().write).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_close(cx)
    }
}

/// A handle to a UNIX-domain listener bound inside the guest by
/// [`PipetteClient::relay_unix_socket`].
///
/// Each call to [`accept`](Self::accept) waits for the next peer to connect
/// to the in-guest listener and yields a [`PipeDuplex`] for that connection,
/// mirroring a plain listener's accept loop. Dropping the listener lets
/// pipette tear the in-guest listener down.
pub struct RelayListener {
    connections: mesh::Receiver<(ReadPipe, WritePipe)>,
}

impl RelayListener {
    /// Waits for the next peer to connect to the in-guest listener, returning
    /// a duplex byte stream for that connection.
    ///
    /// This is distinct from the guest merely having *bound* the listener
    /// (which has already happened by the time [`RelayListener`] is returned):
    /// it resolves only once a peer actually connects, so a caller can wait
    /// for e.g. a guest that has yet to boot.
    pub async fn accept(&mut self) -> anyhow::Result<PipeDuplex> {
        let (read, write) = self
            .connections
            .recv()
            .await
            .context("relay listener closed before a peer connected")?;
        Ok(PipeDuplex { read, write })
    }
}

impl PipetteClient {
    /// Asks the agent to bind a UNIX-domain listener at `bind_path` and relay
    /// each accepted connection back to the host.
    ///
    /// The RPC ack returns once pipette has successfully bound the listener,
    /// so callers can be sure the listener exists before asking another
    /// guest-side process to connect to it. Use
    /// [`RelayListener::accept`] to obtain a [`PipeDuplex`] for each peer that
    /// connects; the first such duplex is typically handed to
    /// [`PipetteClient::new`].
    pub async fn relay_unix_socket(&self, bind_path: &str) -> anyhow::Result<RelayListener> {
        let (connections_send, connections_recv) = mesh::channel();

        self.send
            .call_failable(
                PipetteRequest::RelayUnixSocket,
                RelayUnixSocketRequest {
                    bind_path: bind_path.to_owned(),
                    connections: connections_send,
                },
            )
            .await
            .context("failed to start relay-unix-socket")?;

        Ok(RelayListener {
            connections: connections_recv,
        })
    }

    /// Asks the agent to connect to an existing UNIX-domain socket at
    /// `connect_path` and pump bytes between that connection and the
    /// returned duplex stream.
    ///
    /// This is the complement of [`relay_unix_socket`](Self::relay_unix_socket):
    /// instead of binding a new listener, pipette connects to a socket
    /// that some other guest process has already created (e.g. an in-L1
    /// openvmm's ttrpc control socket).
    pub async fn relay_connect_unix_socket(
        &self,
        connect_path: &str,
    ) -> anyhow::Result<PipeDuplex> {
        let (peer_read, host_write) = mesh::pipe::pipe();
        let (host_read, peer_write) = mesh::pipe::pipe();

        self.send
            .call_failable(
                PipetteRequest::RelayConnectUnixSocket,
                RelayConnectUnixSocketRequest {
                    connect_path: connect_path.to_owned(),
                    to_socket: peer_read,
                    from_socket: peer_write,
                },
            )
            .await
            .context("failed to start relay-connect-unix-socket")?;

        Ok(PipeDuplex {
            read: host_read,
            write: host_write,
        })
    }
}
