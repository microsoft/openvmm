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

/// Whether a VM restored from saved state is the VM that was saved, or a
/// clone of it.
#[derive(MeshPayload, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreKind {
    /// The same VM that was saved, continuing where it left off.
    SameVm,
    /// A new copy of the saved VM.
    Clone,
}

/// Parameters for restoring the VM from saved state.
#[derive(MeshPayload)]
pub struct RestoreParameters {
    /// The saved state to restore.
    pub saved_state: ProtobufMessage,
    /// Whether the restored VM is the saved VM or a clone of it.
    pub kind: RestoreKind,
}

/// Launch parameters for the VM worker.
#[derive(MeshPayload)]
pub struct VmWorkerParameters {
    /// The hypervisor to use.
    pub hypervisor: Resource<HypervisorKind>,
    /// The initial configuration.
    pub cfg: Config,
    /// Restore the VM from saved state instead of booting it, if set.
    pub restore: Option<RestoreParameters>,
    /// File-backed guest RAM handle. When set, guest memory uses this
    /// fd/handle instead of allocating anonymous memory.
    pub shared_memory: Option<SharedMemoryFd>,
    /// The VM RPC channel.
    pub rpc: mesh::Receiver<VmRpc>,
    /// The notification channel.
    pub notify: mesh::Sender<HaltReason>,
}
