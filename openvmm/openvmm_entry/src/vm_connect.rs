// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Attach protocol for connecting a REPL to an already running VM.

use crate::repl;
use crate::vm_controller::VmControllerRpc;
use anyhow::Context;
use futures::FutureExt;
use futures::StreamExt;
use mesh::rpc::RpcSend;
use nvme_resources::NvmeControllerRequest;
use openvmm_defs::rpc::VmRpc;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::path::Path;
use std::path::PathBuf;
use storvsp_resources::ScsiControllerRequest;

#[derive(mesh::MeshPayload)]
pub struct VmConnectRequest {
    pub response: mesh::OneshotSender<Result<VmConnectResponse, mesh::error::RemoteError>>,
}

#[derive(mesh::MeshPayload)]
pub struct VmConnectResponse {
    pub vm_rpc: mesh::Sender<VmRpc>,
    pub vm_controller: mesh::Sender<VmControllerRpc>,
    pub scsi_rpc: Option<mesh::Sender<ScsiControllerRequest>>,
    pub nvme_vtl2_rpc: Option<mesh::Sender<NvmeControllerRequest>>,
    pub shutdown_ic: Option<mesh::Sender<hyperv_ic_resources::shutdown::ShutdownRpc>>,
    pub kvp_ic: Option<mesh::Sender<hyperv_ic_resources::kvp::KvpConnectRpc>>,
    pub has_vtl2: bool,
}

impl VmConnectResponse {
    pub fn into_repl_resources(self) -> repl::ReplResources {
        let (send, recv) = mesh::channel();
        drop(send);
        repl::ReplResources {
            vm_rpc: self.vm_rpc,
            vm_controller: self.vm_controller,
            vm_controller_events: recv,
            scsi_rpc: self.scsi_rpc,
            nvme_vtl2_rpc: self.nvme_vtl2_rpc,
            shutdown_ic: self.shutdown_ic,
            kvp_ic: self.kvp_ic,
            console_in: None,
            has_vtl2: self.has_vtl2,
            quit_behavior: repl::ReplQuitBehavior::Detach,
        }
    }
}

#[derive(Clone, mesh::MeshPayload)]
pub struct AttachResources {
    pub vm_rpc: mesh::Sender<VmRpc>,
    pub vm_controller: mesh::Sender<VmControllerRpc>,
    pub scsi_rpc: Option<mesh::Sender<ScsiControllerRequest>>,
    pub nvme_vtl2_rpc: Option<mesh::Sender<NvmeControllerRequest>>,
    pub shutdown_ic: Option<mesh::Sender<hyperv_ic_resources::shutdown::ShutdownRpc>>,
    pub kvp_ic: Option<mesh::Sender<hyperv_ic_resources::kvp::KvpConnectRpc>>,
    pub has_vtl2: bool,
}

impl From<AttachResources> for VmConnectResponse {
    fn from(resources: AttachResources) -> Self {
        Self {
            vm_rpc: resources.vm_rpc,
            vm_controller: resources.vm_controller,
            scsi_rpc: resources.scsi_rpc,
            nvme_vtl2_rpc: resources.nvme_vtl2_rpc,
            shutdown_ic: resources.shutdown_ic,
            kvp_ic: resources.kvp_ic,
            has_vtl2: resources.has_vtl2,
        }
    }
}

pub struct AttachListener {
    stop: mesh::OneshotSender<()>,
    task: Task<()>,
    path: PathBuf,
}

impl AttachListener {
    pub async fn shutdown(self) {
        self.stop.send(());
        self.task.await;
        crate::cleanup_socket(&self.path);
    }
}

/// Start a mesh listener that hands out VM control channels to connecting
/// clients.
///
/// Returns a handle that must be kept alive. Call [`AttachListener::shutdown`]
/// to stop accepting, shut down the mesh, join the listener task, and remove
/// the socket file.
pub async fn start_attach_listener(
    mesh: &mesh_process::Mesh,
    driver: &impl Spawn,
    path: &Path,
    vm_controller: mesh::Sender<VmControllerRpc>,
) -> anyhow::Result<AttachListener> {
    let mut listener = mesh
        .listen::<VmConnectRequest>(path)
        .await
        .with_context(|| format!("failed to listen on attach socket {}", path.display()))?;
    let (stop_send, stop_recv) = mesh::oneshot::<()>();
    let path = path.to_owned();

    let task = driver.spawn("attach-listener", async move {
        let mut stop_recv = stop_recv.fuse();
        loop {
            enum Action {
                Stop,
                Request(Option<VmConnectRequest>),
            }

            let action = futures::select! {
                _ = stop_recv => Action::Stop,
                request = listener.next().fuse() => Action::Request(request),
            };

            match action {
                Action::Stop => break,
                Action::Request(Some(request)) => {
                    tracing::info!("accepted REPL attach connection");
                    let response = vm_controller
                        .call(VmControllerRpc::Connect, ())
                        .await
                        .unwrap_or_else(|err| Err(mesh::error::RemoteError::new(err)));
                    request.response.send(response);
                }
                Action::Request(None) => break,
            }
        }
    });

    Ok(AttachListener {
        stop: stop_send,
        task,
        path,
    })
}
