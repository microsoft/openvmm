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
    /// The same VM that was saved, continuing where it left off (for example,
    /// guest-transparent hibernation).
    SameVm,
    /// A new copy of the saved VM (for example, a VM created from a template
    /// snapshot, or a rollback to an earlier snapshot).
    Clone,
}

/// Saved state for the VM worker to restore, and how to restore it.
#[derive(MeshPayload)]
pub struct VmRestore {
    /// The saved state.
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
    /// The saved state to restore, if any.
    pub restore: Option<VmRestore>,
    /// File-backed guest RAM handle. When set, guest memory uses this
    /// fd/handle instead of allocating anonymous memory.
    pub shared_memory: Option<SharedMemoryFd>,
    /// The VM RPC channel.
    pub rpc: mesh::Receiver<VmRpc>,
    /// The notification channel.
    pub notify: mesh::Sender<HaltReason>,
}
