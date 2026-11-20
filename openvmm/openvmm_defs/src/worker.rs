// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh worker definitions for the VM worker.

use crate::config::Config;
use crate::rpc::VmRpc;
use hypervisor_resources::HypervisorKind;
use mesh::MeshPayload;
use mesh::payload::message::ProtobufMessage;
use mesh_worker::WorkerId;
use vm_resource::Resource;
use vmm_core_defs::HaltReason;

cfg_select! {
    unix => {
        /// File descriptor (Unix) or handle (Windows) for file-backed guest RAM.
        pub type SharedMemoryFd = std::os::fd::OwnedFd;
    }
    windows => {
        /// File descriptor (Unix) or handle (Windows) for file-backed guest RAM.
        pub type SharedMemoryFd = std::os::windows::io::OwnedHandle;
    }
}

pub const VM_WORKER: WorkerId<VmWorkerParameters> = WorkerId::new("VmWorker");

/// Launch parameters for the VM worker.
#[derive(MeshPayload)]
pub struct VmWorkerParameters {
    /// The hypervisor to use.
    pub hypervisor: Resource<HypervisorKind>,
    /// The initial configuration.
    pub cfg: Config,
    /// The saved state.
    pub saved_state: Option<ProtobufMessage>,
    /// File-backed guest RAM handle. When set, guest memory uses this
    /// fd/handle instead of allocating anonymous memory.
    pub shared_memory: Option<SharedMemoryFd>,
    /// The VM RPC channel.
    pub rpc: mesh::Receiver<VmRpc>,
    /// The notification channel.
    pub notify: mesh::Sender<HaltReason>,
}
