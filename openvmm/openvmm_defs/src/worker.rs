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

/// File descriptor (Unix) or handle (Windows) for file-backed guest RAM.
#[cfg(unix)]
pub type SharedMemoryFd = std::os::fd::OwnedFd;
/// File descriptor (Unix) or handle (Windows) for file-backed guest RAM.
#[cfg(windows)]
pub type SharedMemoryFd = std::os::windows::io::OwnedHandle;

pub const VM_WORKER: WorkerId<VmWorkerParameters> = WorkerId::new("VmWorker");

/// How the VM worker sources guest RAM.
#[derive(MeshPayload)]
pub enum GuestRamSource {
    /// Map this file-backed object as guest RAM. Used for CreateVM with a
    /// backing file, and for restoring a snapshot in place.
    MapShared(SharedMemoryFd),
    /// Populate fresh guest RAM from a snapshot memory file.
    CopyFrom {
        /// The snapshot memory file. Only read; never mapped.
        source: std::fs::File,
        /// File-backed object to use as guest RAM. When `None`, fresh shared
        /// memory is allocated.
        destination: Option<SharedMemoryFd>,
    },
}

/// Launch parameters for the VM worker.
#[derive(MeshPayload)]
pub struct VmWorkerParameters {
    /// The hypervisor to use.
    pub hypervisor: Resource<HypervisorKind>,
    /// The initial configuration.
    pub cfg: Config,
    /// The saved state.
    pub saved_state: Option<ProtobufMessage>,
    /// How guest RAM is sourced. When `None`, fresh memory is allocated
    /// according to `cfg`.
    pub guest_ram: Option<GuestRamSource>,
    /// The VM RPC channel.
    pub rpc: mesh::Receiver<VmRpc>,
    /// The notification channel.
    pub notify: mesh::Sender<HaltReason>,
}
