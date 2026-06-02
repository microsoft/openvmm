// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handler for the `RelayUnixSocket` and `RelayConnectUnixSocket` requests.
//!
//! `RelayUnixSocket` binds a UNIX-domain listener at the requested path,
//! waits for a single connection, and pumps bytes between that connection
//! and the host via a pair of mesh pipes.
//!
//! `RelayConnectUnixSocket` connects to an existing UNIX-domain socket and
//! pumps bytes the same way.
//!
//! See [`pipette_protocol::RelayUnixSocketRequest`] and
//! [`pipette_protocol::RelayConnectUnixSocketRequest`] for the
//! protocol-level contracts.

use anyhow::Context;
use futures::AsyncWriteExt;
use futures_concurrency::future::Race;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pipette_protocol::RelayConnectUnixSocketRequest;
use pipette_protocol::RelayUnixSocketRequest;
use unix_socket::UnixListener;
use unix_socket::UnixStream;

/// Handles a single `RelayUnixSocket` request.
///
/// Binds the listener synchronously so a bind failure is surfaced to the
/// caller through the RPC response. Once the bind succeeds, spawns a
/// detached task that owns the listener, waits for a connection, and runs
/// the pumps; the request future then returns `Ok(())` immediately so the
/// host can proceed without waiting for the peer connect.
pub fn handle_relay_unix_socket(
    driver: &DefaultDriver,
    request: RelayUnixSocketRequest,
) -> anyhow::Result<()> {
    let RelayUnixSocketRequest {
        bind_path,
        to_socket,
        from_socket,
    } = request;

    tracing::debug!(bind_path, "relay-unix-socket bind");

    let listener = UnixListener::bind(&bind_path)
        .with_context(|| format!("failed to bind UNIX listener at {bind_path}"))?;
    let polled = PolledSocket::new(driver, listener)
        .context("failed to create polled listener for relay-unix-socket")?;

    let task_driver = driver.clone();
    driver
        .spawn(
            "relay-unix-socket",
            run_relay(task_driver, bind_path, polled, to_socket, from_socket),
        )
        .detach();
    Ok(())
}

async fn run_relay(
    driver: DefaultDriver,
    bind_path: String,
    mut listener: PolledSocket<UnixListener>,
    to_socket: mesh::pipe::ReadPipe,
    from_socket: mesh::pipe::WritePipe,
) {
    let result = relay_inner(&driver, &mut listener, to_socket, from_socket).await;
    if let Err(err) = result {
        tracing::warn!(
            bind_path,
            error = err.as_ref() as &dyn std::error::Error,
            "relay-unix-socket terminated with error",
        );
    } else {
        tracing::debug!(bind_path, "relay-unix-socket complete");
    }
    drop(listener);
    if let Err(err) = std::fs::remove_file(&bind_path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                bind_path,
                error = &err as &dyn std::error::Error,
                "failed to clean up relay-unix-socket bind path",
            );
        }
    }
}

async fn relay_inner(
    driver: &DefaultDriver,
    listener: &mut PolledSocket<UnixListener>,
    to_socket: mesh::pipe::ReadPipe,
    from_socket: mesh::pipe::WritePipe,
) -> anyhow::Result<()> {
    let (conn, _addr) = listener
        .accept()
        .await
        .context("failed to accept relay connection")?;
    tracing::debug!("relay-unix-socket accepted peer connection");

    let conn =
        PolledSocket::new(driver, conn).context("failed to create polled socket for relay peer")?;

    pump_connection(conn, to_socket, from_socket).await
}

/// Handles a `RelayConnectUnixSocket` request.
///
/// Connects to the socket synchronously so a connect failure is surfaced to
/// the caller through the RPC response. Once the connect succeeds, spawns
/// a detached task that runs the pumps.
pub fn handle_relay_connect_unix_socket(
    driver: &DefaultDriver,
    request: RelayConnectUnixSocketRequest,
) -> anyhow::Result<()> {
    let RelayConnectUnixSocketRequest {
        connect_path,
        to_socket,
        from_socket,
    } = request;

    tracing::debug!(connect_path, "relay-connect-unix-socket connecting");

    let stream = UnixStream::connect(&connect_path)
        .with_context(|| format!("failed to connect to UNIX socket at {connect_path}"))?;
    let conn = PolledSocket::new(driver, stream)
        .context("failed to create polled socket for relay-connect-unix-socket")?;

    let task_driver = driver.clone();
    driver
        .spawn("relay-connect-unix-socket", async move {
            if let Err(err) = pump_connection(conn, to_socket, from_socket).await {
                tracing::warn!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "relay-connect-unix-socket terminated with error",
                );
            } else {
                tracing::debug!("relay-connect-unix-socket complete");
            }
            drop(task_driver);
        })
        .detach();
    Ok(())
}

/// Pump bytes between a connected UNIX stream and a pair of mesh pipes
/// until either side closes.
async fn pump_connection(
    conn: PolledSocket<UnixStream>,
    mut to_socket: mesh::pipe::ReadPipe,
    mut from_socket: mesh::pipe::WritePipe,
) -> anyhow::Result<()> {
    let (mut read_half, mut write_half) = conn.split();

    // Pump bytes in both directions until either side closes. Whichever
    // direction finishes first tears down the relay; the other pump is
    // dropped at function exit.
    enum Done {
        ToPeer(std::io::Result<u64>),
        FromPeer(std::io::Result<u64>),
    }
    let to_peer = async {
        let r = futures::io::copy(&mut to_socket, &mut write_half).await;
        // Send EOF to the peer so it observes the close.
        let _ = write_half.close().await;
        Done::ToPeer(r)
    };
    let from_peer = async {
        let r = futures::io::copy(&mut read_half, &mut from_socket).await;
        // Drop our end of the host pipe so the host observes EOF.
        let _ = from_socket.close().await;
        Done::FromPeer(r)
    };

    match (to_peer, from_peer).race().await {
        Done::ToPeer(r) => {
            r.context("relay-unix-socket host -> peer pump failed")?;
        }
        Done::FromPeer(r) => {
            r.context("relay-unix-socket peer -> host pump failed")?;
        }
    }
    Ok(())
}
