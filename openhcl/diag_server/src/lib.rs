// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Underhill diagnostics server.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

mod diag_service;

pub use diag_service::DiagRequest;
pub use diag_service::StartParams;

use anyhow::Context;
use cvm_tracing::CVM_ALLOWED;
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
        tracing::info!(CVM_ALLOWED, ?control_address, "control starting");
        let control_listener =
            VmListener::bind(control_address).context("failed to bind socket")?;

        tracing::info!(CVM_ALLOWED, ?data_address, "data starting");
        let data_listener = VmListener::bind(data_address).context("failed to bind socket")?;

        Ok(Self::new_generic(
            control_listener.into(),
            data_listener.into(),
        ))
    }

    /// Creates a server over Unix sockets and starts listening.
    pub fn new_unix(control_address: &Path, data_address: &Path) -> anyhow::Result<Self> {
        tracing::info!(CVM_ALLOWED, ?control_address, "control starting");
        let control_listener =
            UnixListener::bind(control_address).context("failed to bind socket")?;

        tracing::info!(CVM_ALLOWED, ?data_address, "data starting");
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

    /// Serves requests until `cancel` is signalled or its sender is dropped.
    ///
    /// Like the diagnostics worker's immediate `WorkerRpc::Stop` handling,
    /// shutdown drops the service loops without draining detached requests.
    pub async fn serve(
        mut self,
        driver: &(impl Driver + Spawn + Clone),
        cancel: mesh::OneshotReceiver<()>,
        request_send: mesh::Sender<DiagRequest>,
    ) -> anyhow::Result<()> {
        // Disable all diag requests for CVMs. Inspect filtering will be handled
        // internally more granularly.
        let (diag_recv, diag2_recv) = if underhill_confidentiality::confidential_filtering_enabled()
        {
            (RpcReceiver::disconnected(), RpcReceiver::disconnected())
        } else {
            (
                self.server.add_service::<diag_proto::UnderhillDiag>(),
                self.server.add_service::<diag_proto::OpenhclDiag>(),
            )
        };

        let inspect_recv = self.server.add_service::<inspect_proto::InspectService>();

        // TODO: split the profiler to a separate service provider.
        let profile_recv = self
            .server
            .add_service::<azure_profiler_proto::AzureProfiler>();

        let diag_service = Arc::new(diag_service::DiagServiceHandler::new(
            request_send,
            self.inner.clone(),
        ));
        let process = diag_service.process_requests(
            driver,
            diag_recv,
            diag2_recv,
            inspect_recv,
            profile_recv,
        );

        let serve = self.server.run(driver, self.control_listener, cancel);
        let data_connections = self
            .inner
            .process_data_connections(driver, self.data_listener);

        let supporting = futures::future::try_join(process, data_connections);
        let mut supporting = pin!(supporting.fuse());
        let mut serve = pin!(serve.fuse());
        futures::select! { // race semantics
            result = serve => result,
            result = supporting => result.map(|_| ()),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncReadExt;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::timer::PolledTimer;
    use std::future::Future;
    use std::time::Duration;
    use test_with_tracing::test;

    async fn expect_shutdown_after_cancel(
        driver: &DefaultDriver,
        future: impl Future<Output = anyhow::Result<()>>,
        cancel: mesh::OneshotSender<()>,
    ) {
        let mut serve = pin!(future.fuse());
        assert!(
            futures::poll!(serve.as_mut()).is_pending(),
            "the server must be running before cancellation"
        );
        drop(cancel);

        let mut timer = PolledTimer::new(driver);
        let mut timeout = pin!(timer.sleep(Duration::from_secs(1)).fuse());
        futures::select_biased! {
            result = serve => result.expect("the server must shut down without an error"),
            _ = timeout => panic!("server did not shut down within one second after cancellation"),
        }
    }

    // Starts a full diagnostics server on local Unix sockets, then cancels with no clients.
    // Expects serve to finish within one second and release both listening sockets.
    #[async_test]
    async fn test_diag_server_shutdown_after_cancel(driver: DefaultDriver) {
        let directory = tempfile::tempdir().unwrap();
        let control = directory.path().join("control.sock");
        let data = directory.path().join("data.sock");
        let server = DiagServer::new_unix(&control, &data).unwrap();
        let (cancel, cancelled) = mesh::oneshot();
        let (requests, _request_recv) = mesh::channel();

        expect_shutdown_after_cancel(&driver, server.serve(&driver, cancelled, requests), cancel)
            .await;

        assert!(unix_socket::UnixStream::connect(&control).is_err());
        assert!(unix_socket::UnixStream::connect(&data).is_err());
    }

    // Opens control and data connections, completes the data handshake, then signals cancellation.
    // Expects shutdown to close both connections and release both listening sockets.
    #[async_test]
    async fn test_diag_server_shutdown_with_connections(driver: DefaultDriver) {
        let directory = tempfile::tempdir().unwrap();
        let control = directory.path().join("control.sock");
        let data = directory.path().join("data.sock");
        let server = DiagServer::new_unix(&control, &data).unwrap();
        let (cancel, cancelled) = mesh::oneshot();
        let (requests, _request_recv) = mesh::channel();
        let mut control_client =
            PolledSocket::new(&driver, unix_socket::UnixStream::connect(&control).unwrap())
                .unwrap();
        let mut data_client =
            PolledSocket::new(&driver, unix_socket::UnixStream::connect(&data).unwrap()).unwrap();

        let operation = async {
            let mut serve = pin!(server.serve(&driver, cancelled, requests).fuse());
            let mut id = [0; 8];
            futures::select! {
                result = serve => panic!("server stopped before the data handshake: {result:?}"),
                result = data_client.read_exact(&mut id).fuse() => result.unwrap(),
            }
            cancel.send(());
            serve
                .await
                .expect("the server must shut down without an error");

            let mut byte = [0];
            assert_eq!(control_client.read(&mut byte).await.unwrap(), 0);
            assert_eq!(data_client.read(&mut byte).await.unwrap(), 0);
            assert!(unix_socket::UnixStream::connect(&control).is_err());
            assert!(unix_socket::UnixStream::connect(&data).is_err());
        };
        let mut operation = pin!(operation.fuse());
        let mut timer = PolledTimer::new(&driver);
        let mut timeout = pin!(timer.sleep(Duration::from_secs(1)).fuse());
        futures::select_biased! {
            _ = operation => {},
            _ = timeout => panic!("connected server did not shut down within one second"),
        }
    }

    // Runs the server with a data socket that cannot accept connections and no cancellation.
    // Expects the data-listener error to reach the caller rather than report successful shutdown.
    #[async_test]
    async fn test_diag_server_data_listener_error(driver: DefaultDriver) {
        let directory = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(directory.path().join("control.sock")).unwrap();
        let data = Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap();
        let server = DiagServer::new_generic(listener.into(), data);
        let (_cancel, cancelled) = mesh::oneshot();
        let (requests, _request_recv) = mesh::channel();
        let mut serve = pin!(server.serve(&driver, cancelled, requests).fuse());
        let mut timer = PolledTimer::new(&driver);
        let mut timeout = pin!(timer.sleep(Duration::from_secs(1)).fuse());
        let error = futures::select_biased! {
            result = serve => result.expect_err("the data-listener failure must propagate"),
            _ = timeout => panic!("the server did not report its data-listener failure"),
        };
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    // Starts only the underlying control RPC server, then drops its cancellation sender.
    // Expects shutdown within one second and release of its listening socket.
    #[async_test]
    async fn test_control_server_shutdown_after_cancel(driver: DefaultDriver) {
        let directory = tempfile::tempdir().unwrap();
        let control = directory.path().join("control.sock");
        let listener = UnixListener::bind(&control).unwrap();
        let mut server = mesh_rpc::Server::new();
        let (cancel, cancelled) = mesh::oneshot();

        expect_shutdown_after_cancel(&driver, server.run(&driver, listener, cancelled), cancel)
            .await;

        assert!(unix_socket::UnixStream::connect(&control).is_err());
    }
}
