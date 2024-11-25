// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Underhill diagnostics server.

#![cfg(target_os = "linux")]
#![warn(missing_docs)]

mod diag_service;
mod new_pty;

pub use diag_service::DiagRequest;
pub use diag_service::StartParams;

use anyhow::Context;
use futures::AsyncWriteExt;
use futures::FutureExt;
use mesh::CancelReason;
use mesh_rpc::server::RpcReceiver;
use mesh_rpc::service::Code;
use mesh_rpc::service::Status;
use pal_async::driver::Driver;
use pal_async::interest::PollEvents;
use pal_async::socket::PollReadyExt;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use socket2::Socket;
use std::collections::HashMap;
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;
use unix_socket::UnixListener;
use vmsocket::VmAddress;
use vmsocket::VmListener;

/// The diagnostics server, which is a ttrpc server listening on `AF_VSOCK` at
/// for control and data.
pub struct DiagServer {
    // control listener
    control_listener: Socket,
    // data listener
    data_listener: Socket,
    inner: Arc<Inner>,
    server: mesh_rpc::Server,
}

impl DiagServer {
    /// Creates a server over VmSockets and starts listening.
    pub fn new_vsock(control_address: VmAddress, data_address: VmAddress) -> anyhow::Result<Self> {
        tracing::info!(?control_address, "control starting");
        let control_listener =
            VmListener::bind(control_address).context("failed to bind socket")?;

        tracing::info!(?data_address, "data starting");
        let data_listener = VmListener::bind(data_address).context("failed to bind socket")?;

        Ok(Self::new_generic(
            control_listener.into(),
            data_listener.into(),
        ))
    }

    /// Creates a server over Unix sockets and starts listening.
    pub fn new_unix(control_address: &Path, data_address: &Path) -> anyhow::Result<Self> {
        tracing::info!(?control_address, "control starting");
        let control_listener =
            UnixListener::bind(control_address).context("failed to bind socket")?;

        tracing::info!(?data_address, "data starting");
        let data_listener = UnixListener::bind(data_address).context("failed to bind socket")?;

        Ok(Self::new_generic(
            control_listener.into(),
            data_listener.into(),
        ))
    }

    fn new_generic(control_listener: Socket, data_listener: Socket) -> Self {
        Self {
            control_listener,
            data_listener,
            server: mesh_rpc::Server::new(),
            inner: Arc::new(Inner {
                connections: Mutex::new(DataConnections {
                    next_id: 1, // connection IDs start at 1, as 0 is an invalid ID.
                    active: Default::default(),
                }),
            }),
        }
    }

    /// Serves requests until `cancel` is dropped.
    pub async fn serve(
        mut self,
        driver: &(impl Driver + Spawn + Clone),
        cancel: mesh::OneshotReceiver<()>,
        request_send: mesh::Sender<DiagRequest>,
    ) -> anyhow::Result<()> {
        // Disable all diag requests for CVMs. Inspect filtering will be handled
        // internally more granularly.
        let diag_recv = if underhill_confidentiality::confidential_filtering_enabled() {
            RpcReceiver::disconnected()
        } else {
            self.server.add_service()
        };

        let inspect_recv = self.server.add_service();

        // TODO: split the profiler to a separate service provider.
        let profile_recv = self.server.add_service();

        let diag_service = Arc::new(diag_service::DiagServiceHandler::new(
            request_send,
            self.inner.clone(),
        ));
        let process = diag_service.process_requests(driver, diag_recv, inspect_recv, profile_recv);

        let serve = self.server.run(driver, self.control_listener, cancel);
        let data_connections = self
            .inner
            .process_data_connections(driver, self.data_listener);

        futures::future::try_join3(serve, process, data_connections).await?;
        Ok(())
    }
}

#[derive(Debug)]
struct DataConnectionEntry {
    /// Sender used to notify the hangup task to return the socket.
    sender: mesh::OneshotSender<()>,
    /// Task used to wait for hangup notifications or a request to return the socket.
    task: Task<Option<PolledSocket<Socket>>>,
}

#[derive(Debug, Default)]
struct DataConnections {
    next_id: u64,
    active: HashMap<u64, DataConnectionEntry>,
}

impl DataConnections {
    fn take_connection(&mut self, id: u64) -> anyhow::Result<DataConnectionEntry> {
        self.active
            .remove(&id)
            .ok_or_else(|| anyhow::anyhow!("invalid connection id"))
    }
}

struct Inner {
    connections: Mutex<DataConnections>,
}

impl Inner {
    async fn take_connection(&self, id: u64) -> anyhow::Result<PolledSocket<Socket>> {
        let DataConnectionEntry { sender, task } = self.connections.lock().take_connection(id)?;

        sender.send(());
        task.await
            .ok_or_else(|| anyhow::anyhow!("connection disconnected"))
    }

    /// Listen for data connections and add them to the internal connections lookup table as they arrive.
    async fn process_data_connections(
        self: &Arc<Self>,
        driver: &(impl Driver + Spawn + Clone),
        listener: Socket,
    ) -> anyhow::Result<()> {
        let mut listener = PolledSocket::new(driver, listener)?;

        loop {
            let (connection, _addr) = listener.accept().await?;
            let mut socket = PolledSocket::new(driver, connection)?;
            let inner = Arc::downgrade(self);

            // Send the 8 byte connection id, then stash the connection in the lookup table to be used later.
            let id;
            {
                let mut state = self.connections.lock();
                id = state.next_id;
                state.next_id += 1;

                tracing::debug!(id, "new data connection");
            }

            let (sender, recv) = mesh::oneshot();

            // Spawn a task that returns the socket when asked to, or removes itself from the map if disconnected.
            let task = driver.spawn(format!("data connection {} waiting", id), async move {
                match socket.write_all(&id.to_ne_bytes()).await {
                    Ok(_) => {}
                    Err(error) => {
                        tracing::trace!(?error, "error writing connection id, removing.");
                        if let Some(state) = inner.upgrade() {
                            state.connections.lock().active.remove(&id);
                        }

                        return None;
                    }
                }

                let mut return_future = pin!(async { recv.await.is_ok() }.fuse());
                let hangup = futures::select! { // race semantics
                    _ = socket.wait_ready(PollEvents::RDHUP).fuse() => true,
                    _ = return_future => false,
                };

                if hangup {
                    // Other side has disconnected, remove from the table if not already done.
                    tracing::trace!(id, "data connection disconnected");
                    if let Some(state) = inner.upgrade() {
                        state.connections.lock().active.remove(&id);
                    }

                    None
                } else {
                    Some(socket)
                }
            });

            let mut state = self.connections.lock();
            let result = state
                .active
                .insert(id, DataConnectionEntry { sender, task });

            if result.is_some() {
                anyhow::bail!("connection id reused");
            }
        }
    }
}

fn grpc_result<T>(result: Result<anyhow::Result<T>, CancelReason>) -> Result<T, Status> {
    match result {
        Ok(result) => match result {
            Ok(value) => Ok(value),
            Err(err) => Err(Status {
                code: Code::Unknown as i32,
                message: format!("{:#}", err),
                details: vec![],
            }),
        },
        Err(err) => Err(Status {
            code: match &err {
                CancelReason::Cancelled => Code::Cancelled,
                CancelReason::DeadlineExceeded => Code::DeadlineExceeded,
            } as i32,
            message: format!("{:#}", err),
            details: vec![],
        }),
    }
}
