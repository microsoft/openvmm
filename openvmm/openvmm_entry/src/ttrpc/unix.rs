// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix RPC transport setup and fd-passing dispatch.

use super::ResolvedTransport;
use super::bind_unix_listener;
use super::dispatch;
use super::fd_passing;
pub(super) use super::fd_passing::FdRegistry;
use pal_async::driver::Driver;
use pal_async::socket::PolledSocket;
use unix_socket::UnixListener;
use unix_socket::UnixStream;

#[derive(mesh::MeshPayload)]
pub enum Listener {
    Unix(UnixListener),
}

pub fn listener_from_path(
    path: &std::path::Path,
    allow_sid: Option<&str>,
) -> anyhow::Result<Listener> {
    anyhow::ensure!(
        allow_sid.is_none(),
        "allow_sid is only supported on Windows"
    );
    Ok(Listener::Unix(bind_unix_listener(path)?))
}

pub(super) enum BoundListener {
    Unix(UnixListener),
}

impl BoundListener {
    pub(super) fn new(listener: Listener) -> anyhow::Result<Self> {
        let Listener::Unix(listener) = listener;
        Ok(Self::Unix(listener))
    }

    pub(super) async fn run(
        self,
        server: &mesh_rpc::Server,
        driver: &(impl Driver + ?Sized),
        cancel: mesh::OneshotReceiver<()>,
        transport: ResolvedTransport,
        registry: FdRegistry,
    ) -> anyhow::Result<()> {
        let Self::Unix(listener) = self;
        dispatch::run(server, driver, listener, cancel, transport, registry).await
    }
}

pub(super) async fn serve_socket(
    server: &mesh_rpc::Server,
    conn: PolledSocket<UnixStream>,
    first_byte: u8,
    transport: ResolvedTransport,
    registry: &FdRegistry,
) -> anyhow::Result<()> {
    // The fd-passing protocol is allowed in every RPC transport mode.
    if first_byte == fd_passing::MAGIC_FIRST_BYTE {
        return fd_passing::serve(conn, registry).await;
    }
    dispatch::serve_by_protocol(
        server,
        conn,
        first_byte,
        transport,
        mesh_rpc::server::ShutdownPolicy::Close,
    )
    .await
}
