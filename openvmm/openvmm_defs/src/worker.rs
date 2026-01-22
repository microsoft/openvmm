// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh worker definitions for the VM worker.

use crate::config::Config;
use crate::config::Hypervisor;
use crate::rpc::VmRpc;
use mesh::MeshPayload;
use mesh::payload::message::ProtobufMessage;
use mesh_worker::WorkerId;
use vmm_core_defs::HaltReason;

pub const VM_WORKER: WorkerId<VmWorkerParameters> = WorkerId::new("VmWorker");

/// Launch parameters for the VM worker.
#[derive(MeshPayload)]
pub struct VmWorkerParameters {
    /// The hypervisor to use.
    pub hypervisor: Option<Hypervisor>,
    /// The initial configuration.
    pub cfg: Config,
    /// The saved state.
    pub saved_state: Option<ProtobufMessage>,
    /// The VM RPC channel.
    pub rpc: mesh::Receiver<VmRpc>,
    /// The notification channel.
    pub notify: mesh::Sender<HaltReason>,
}
