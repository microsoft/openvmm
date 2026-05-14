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

impl PipetteClient {
    /// Asks the agent to bind a UNIX-domain listener at `bind_path`, wait
    /// for a single peer connection, and pump bytes between that peer and
    /// the returned duplex stream.
    ///
    /// The returned [`PipeDuplex`] is intended to be handed to
    /// [`PipetteClient::new`] (or any other consumer of an
    /// `AsyncRead + AsyncWrite` byte stream).
    ///
    /// The RPC ack returns once pipette has successfully bound the
    /// listener, so callers can be sure the listener exists before
    /// asking another guest-side process to connect to it.
    pub async fn relay_unix_socket(&self, bind_path: &str) -> anyhow::Result<PipeDuplex> {
        // Pair 1: host writes -> pipette reads -> peer receives.
        let (peer_read, host_write) = mesh::pipe::pipe();
        // Pair 2: peer sends -> pipette writes -> host reads.
        let (host_read, peer_write) = mesh::pipe::pipe();

        self.send
            .call_failable(
                PipetteRequest::RelayUnixSocket,
                RelayUnixSocketRequest {
                    bind_path: bind_path.to_owned(),
                    to_socket: peer_read,
                    from_socket: peer_write,
                },
            )
            .await
            .context("failed to start relay-unix-socket")?;

        Ok(PipeDuplex {
            read: host_read,
            write: host_write,
        })
    }
}
