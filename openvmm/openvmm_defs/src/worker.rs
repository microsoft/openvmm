// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh worker definitions for the VM worker.

use crate::config::Config;
use crate::rpc::VmRpc;
use hypervisor_resources::HypervisorKind;
use mesh::MeshPayload;
use mesh::payload::Protobuf;
use mesh::payload::message::ProtobufMessage;
use mesh_worker::WorkerId;
use state_unit::SavedStateUnit;
use vm_resource::Resource;
use vmcore::save_restore::SavedStateRoot;
use vmm_core_defs::HaltReason;

/// File descriptor (Unix) or handle (Windows) for file-backed guest RAM.
#[cfg(unix)]
pub type SharedMemoryFd = std::os::fd::OwnedFd;
/// File descriptor (Unix) or handle (Windows) for file-backed guest RAM.
#[cfg(windows)]
pub type SharedMemoryFd = std::os::windows::io::OwnedHandle;

pub const VM_WORKER: WorkerId<VmWorkerParameters> = WorkerId::new("VmWorker");

/// Complete event written before a restored VM can execute.
pub const RESTORE_READY_EVENT_V1: &[u8] = b"OPENVMM_RESTORE_READY_V1\n";

/// Exact snapshot-generation handles retained for a restored VM's lifetime.
#[derive(MeshPayload)]
pub struct SnapshotRestoreGuards {
    /// Open snapshot directory used for relative artifact access and resume claims.
    pub directory: std::fs::File,
    /// Open manifest artifact.
    pub manifest: std::fs::File,
    /// Open saved-state artifact.
    pub state: std::fs::File,
    /// Open memory artifact backing the copy-on-write section.
    pub memory: std::fs::File,
}

/// Complete saved state consumed by the VM worker.
#[derive(Protobuf, SavedStateRoot)]
#[mesh(package = "openvmm")]
pub struct SavedState {
    #[mesh(1)]
    pub units: Vec<SavedStateUnit>,
    /// Complete state-unit inventory, including units with no mutable state.
    #[mesh(2)]
    pub inventory: Vec<String>,
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
    /// File-backed guest RAM handle. When set, guest memory uses this
    /// fd/handle instead of allocating anonymous memory.
    pub shared_memory: Option<SharedMemoryFd>,
    /// Whether writes to `shared_memory` must remain private to this VM.
    pub shared_memory_copy_on_write: bool,
    /// Snapshot generation handles that must outlive the restored VM.
    pub snapshot_restore_guards: Option<SnapshotRestoreGuards>,
    /// Deferred microVM PMIO requests awaiting an exact post-OUT boundary.
    pub snapshot_boundary_requests:
        Option<mesh::Receiver<chipset_resources::microvm::MicrovmSnapshotBoundaryRequest>>,
    /// Notifies the controller after the worker establishes the boundary.
    pub snapshot_ready:
        Option<mesh::Sender<chipset_resources::microvm::MicrovmSnapshotScratchPolicy>>,
    /// Whether this cold boot can publish a microVM snapshot.
    pub snapshot_capture_enabled: bool,
    /// Host downtime to apply before starting a restored VM.
    pub restore_downtime: Option<std::time::Duration>,
    /// Saved effective TSC frequency required by restore.
    pub restore_tsc_frequency_hz: Option<u64>,
    /// Saved local APIC timer frequency required by restore.
    pub restore_apic_frequency_hz: Option<u64>,
    /// Saved canonical CPU contract required by restore.
    pub restore_cpu_contract: Option<Vec<u8>>,
    /// Single-use process-local sink for the restore readiness event.
    pub restore_ready_sink: Option<std::fs::File>,
    /// Timeout for the microVM post-restore input gate, when required.
    pub restore_gate_timeout: Option<std::time::Duration>,
    /// MSHV VP prefix instantiated for an explicit restore-time activation target.
    pub restore_vp_count: Option<u32>,
    /// The VM RPC channel.
    pub rpc: mesh::Receiver<VmRpc>,
    /// The notification channel.
    pub notify: mesh::Sender<HaltReason>,
}
