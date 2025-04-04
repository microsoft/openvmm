// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Underhill diagnostics server worker.

use anyhow::Context;
use diag_server::DiagServer;
use futures::FutureExt;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use pal_async::DefaultPool;
use std::pin::pin;
use vmsocket::VmAddress;

/// The worker ID.
pub const DIAG_WORKER: WorkerId<DiagWorkerParameters> = WorkerId::new("DiagWorker");

/// The diagnostics server worker.
pub struct DiagWorker {
    server: DiagServer,
    request_send: mesh::Sender<diag_server::DiagRequest>,
}

/// The parameters for [`DiagWorker`].
#[derive(MeshPayload)]
pub struct DiagWorkerParameters {
    /// A channel to send requests to.
    pub request_send: mesh::Sender<diag_server::DiagRequest>,
}

impl Worker for DiagWorker {
    type Parameters = DiagWorkerParameters;

    type State = ();

    const ID: WorkerId<Self::Parameters> = DIAG_WORKER;

    fn new(parameters: Self::Parameters) -> anyhow::Result<Self> {
        let server = DiagServer::new_vsock(
            VmAddress::vsock_any(diag_proto::VSOCK_CONTROL_PORT),
            VmAddress::vsock_any(diag_proto::VSOCK_DATA_PORT),
        )
        .context("failed to create diagnostics server")?;
        Ok(Self {
            server,
            request_send: parameters.request_send,
        })
    }

    fn restart(_state: Self::State) -> anyhow::Result<Self> {
        unimplemented!()
    }

    fn run(self, mut recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        DefaultPool::run_with(async |driver| {
            let (_cancel_send, cancel) = mesh::oneshot();
            let mut serve = pin!(self.server.serve(&driver, cancel, self.request_send).fuse());
            loop {
                let msg = futures::select! { // merge semantics
                    msg = recv.recv().fuse() => msg.context("worker handle closed")?,
                    r = serve => break r,
                };
                match msg {
                    WorkerRpc::Stop => break Ok(()),
                    WorkerRpc::Restart(rpc) => {
                        rpc.complete(Err(RemoteError::new(anyhow::anyhow!("not supported"))));
                    }
                    WorkerRpc::Inspect(_) => {}
                }
            }
        })
    }
}
