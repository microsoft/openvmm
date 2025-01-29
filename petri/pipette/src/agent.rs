// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The main pipette agent, which is run when the process starts.

#![cfg(any(target_os = "linux", target_os = "windows"))]

use anyhow::Context;
use futures::future::FutureExt;
use futures_concurrency::future::RaceOk;
use mesh_remote::PointToPointMesh;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::timer::PolledTimer;
use pal_async::DefaultDriver;
use pipette_protocol::DiagnosticFile;
use pipette_protocol::PipetteBootstrap;
use pipette_protocol::PipetteRequest;
use socket2::Socket;
use std::sync::Arc;
use std::time::Duration;
use unicycle::FuturesUnordered;
use vmsocket::VmAddress;
use vmsocket::VmSocket;

pub struct Agent {
    driver: DefaultDriver,
    mesh: PointToPointMesh,
    request_recv: mesh::Receiver<PipetteRequest>,
    diag_file_send: DiagnosticSender,
    watch_send: mesh::OneshotSender<()>,
}

#[allow(dead_code)] // Not used on all platforms yet
#[derive(Clone)]
pub struct DiagnosticSender(Arc<mesh::Sender<DiagnosticFile>>);

impl Agent {
    pub async fn new(driver: DefaultDriver) -> anyhow::Result<Self> {
        // These shouldn't need `.fuse()`, but without it the code panics with
        // 'async fn' resumed after completion.
        // https://github.com/yoshuawuyts/futures-concurrency/pull/204
        let socket = (
            connect_client(&driver).fuse(),
            connect_server(&driver).fuse(),
        )
            .race_ok()
            .await
            .map_err(|e| {
                let [e0, e1] = &*e;
                anyhow::anyhow!(
                    "failed to connect. client error: {:#} server error: {:#}",
                    e0,
                    e1
                )
            })?;

        let (bootstrap_send, bootstrap_recv) = mesh::oneshot::<PipetteBootstrap>();
        let mesh = PointToPointMesh::new(&driver, socket, bootstrap_recv.into());

        let (request_send, request_recv) = mesh::channel();
        let (diag_file_send, diag_file_recv) = mesh::channel();
        let (watch_send, watch_recv) = mesh::oneshot();
        let log = crate::trace::init_tracing();

        bootstrap_send.send(PipetteBootstrap {
            requests: request_send,
            diag_file_recv,
            watch: watch_recv,
            log,
        });

        Ok(Self {
            driver,
            mesh,
            request_recv,
            diag_file_send: DiagnosticSender(Arc::new(diag_file_send)),
            watch_send,
        })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut tasks = FuturesUnordered::new();
        loop {
            futures::select! {
                req = self.request_recv.recv().fuse() => {
                    match req {
                        Ok(req) => {
                            tasks.push(handle_request(&self.driver, req, self.diag_file_send.clone()));
                        },
                        Err(e) => {
                            tracing::info!(?e, "request channel closed, shutting down");
                            break;
                        }
                    }
                }
                _ = tasks.next() => {}
            }
        }
        self.watch_send.send(());
        self.mesh.shutdown().await;
        Ok(())
    }
}

async fn connect_server(driver: &DefaultDriver) -> anyhow::Result<PolledSocket<Socket>> {
    let mut socket = VmSocket::new()?;
    socket.bind(VmAddress::vsock_any(pipette_protocol::PIPETTE_VSOCK_PORT))?;
    let mut socket =
        PolledSocket::new(driver, socket.into()).context("failed to create polled socket")?;
    socket.listen(1)?;
    let socket = socket
        .accept()
        .await
        .context("failed to accept connection")?
        .0;
    PolledSocket::new(driver, socket).context("failed to create polled socket")
}

async fn connect_client(driver: &DefaultDriver) -> anyhow::Result<PolledSocket<Socket>> {
    let socket = VmSocket::new()?;
    // Extend the default timeout of 2 seconds, as tests are often run in
    // parallel on a host, causing very heavy load on the overall system.
    socket
        .set_connect_timeout(Duration::from_secs(5))
        .context("failed to set socket timeout")?;
    let mut socket = PolledSocket::new(driver, socket)
        .context("failed to create polled socket")?
        .convert();
    socket
        .connect(&VmAddress::vsock_host(pipette_protocol::PIPETTE_VSOCK_PORT).into())
        .await?;
    Ok(socket)
}

async fn handle_request(
    driver: &DefaultDriver,
    req: PipetteRequest,
    _diag_file_send: DiagnosticSender, // Not used on all platforms yet
) {
    match req {
        PipetteRequest::Ping(rpc) => rpc.handle_sync(|()| {
            tracing::info!("ping");
        }),
        PipetteRequest::Execute(rpc) => rpc.handle_failable_sync(crate::execute::handle_execute),
        PipetteRequest::Shutdown(rpc) => {
            rpc.handle_sync(|request| {
                tracing::info!(shutdown_type = ?request.shutdown_type, "shutdown request");
                // TODO: handle this inline without waiting. Currently we spawn
                // a task so that the response is sent before the shutdown
                // starts, since hvlite fails to notice that the connection is
                // closed if we power off while a response is pending.
                let mut timer = PolledTimer::new(driver);
                driver
                    .spawn("shutdown", async move {
                        // Because pipette runs as a system service on Windows
                        // it is able to issue a shutdown command before Windows
                        // has finished starting up and logging in the user. This
                        // can put the system into a stuck state, where it is
                        // completely unable to shut down. To avoid this, we
                        // wait for a longer period before attempting to shut down.
                        #[cfg(windows)]
                        timer.sleep(Duration::from_secs(5)).await;
                        #[cfg(not(windows))]
                        timer.sleep(Duration::from_millis(250)).await;
                        loop {
                            if let Err(err) = crate::shutdown::handle_shutdown(request) {
                                tracing::error!(
                                    error = err.as_ref() as &dyn std::error::Error,
                                    "failed to shut down"
                                );
                            }
                            timer.sleep(Duration::from_secs(5)).await;
                            tracing::warn!("still waiting to shut down, trying again");
                        }
                    })
                    .detach();
                Ok(())
            })
        }
        PipetteRequest::ReadFile(rpc) => rpc.handle_failable(read_file).await,
        PipetteRequest::WriteFile(rpc) => rpc.handle_failable(write_file).await,
    }
}

async fn read_file(mut request: pipette_protocol::ReadFileRequest) -> anyhow::Result<()> {
    tracing::debug!(path = request.path, "Beginning file read request");
    let file = fs_err::File::open(request.path)?;
    futures::io::copy(&mut futures::io::AllowStdIo::new(file), &mut request.sender).await?;
    tracing::debug!("file read request complete");
    Ok(())
}

async fn write_file(mut request: pipette_protocol::WriteFileRequest) -> anyhow::Result<()> {
    tracing::debug!(path = request.path, "Beginning file write request");
    let file = fs_err::File::create(request.path)?;
    futures::io::copy(
        &mut request.receiver,
        &mut futures::io::AllowStdIo::new(file),
    )
    .await?;
    tracing::debug!("file write request complete");
    Ok(())
}

impl DiagnosticSender {
    #[allow(dead_code)] // Not used on all platforms yet
    pub async fn send(&self, filename: &str) -> anyhow::Result<()> {
        tracing::debug!(filename, "Beginning diagnostic file request");
        let file = fs_err::File::open(filename)?;
        let (recv_pipe, mut send_pipe) = mesh::pipe::pipe();
        self.0.send(DiagnosticFile {
            name: filename.to_owned(),
            receiver: recv_pipe,
        });
        futures::io::copy(&mut futures::io::AllowStdIo::new(file), &mut send_pipe).await?;
        drop(send_pipe);
        tracing::debug!("diagnostic request complete");
        Ok(())
    }
}
