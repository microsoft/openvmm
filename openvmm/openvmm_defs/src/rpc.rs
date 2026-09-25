// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RPC types for communicating with the VM worker.

use crate::config::DeviceVtl;
use guid::Guid;
use mesh::CancelContext;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::payload::message::ProtobufMessage;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;
use std::fmt;
use std::fs::File;
use std::time::Duration;
use vm_resource::Resource;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::kind::VmbusDeviceHandleKind;

#[derive(MeshPayload)]
pub enum VmRpc {
    Save(FailableRpc<(), ProtobufMessage>),
    /// Boundedly quiesce the VM and return saved state while leaving it stopped.
    QuiesceForSnapshot(Rpc<Duration, Result<SnapshotSaveResponse, SnapshotQuiesceError>>),
    /// Resume a VM after a rollback-safe snapshot failure before commit.
    ResumeAfterFailedSnapshot(FailableRpc<Duration, ()>),
    /// Release a post-OUT boundary without starting a snapshot transaction.
    ReleaseSnapshotBoundary(FailableRpc<(), ()>),
    Resume(FailableRpc<(), bool>),
    Pause(Rpc<(), bool>),
    ClearHalt(Rpc<(), bool>),
    Reset(FailableRpc<(), ()>),
    Nmi(Rpc<u32, ()>),
    AddVmbusDevice(FailableRpc<(DeviceVtl, Resource<VmbusDeviceHandleKind>), ()>),
    ConnectHvsock(FailableRpc<(CancelContext, Guid, DeviceVtl), unix_socket::UnixStream>),
    PulseSaveRestore(Rpc<(), Result<(), PulseSaveRestoreError>>),
    StartReloadIgvm(FailableRpc<File, ()>),
    CompleteReloadIgvm(FailableRpc<bool, ()>),
    ReadMemory(FailableRpc<(u64, usize), Vec<u8>>),
    WriteMemory(FailableRpc<(u64, Vec<u8>), ()>),
    /// Updates the command line parameters that will be passed to the boot shim
    /// on the *next* VM load. This will replace the existing command line parameters.
    UpdateCliParams(FailableRpc<String, ()>),
    /// Hot-add a PCIe device to a named port at runtime.
    /// Tuple is (port_name, device_resource).
    AddPcieDevice(FailableRpc<(String, Resource<PciDeviceHandleKind>), ()>),
    /// Hot-remove a PCIe device from a named port at runtime.
    RemovePcieDevice(FailableRpc<String, ()>),
    /// Hot-add a VPCI device to VTL0 at runtime with the supplied instance ID.
    AddVpciDevice(FailableRpc<(Guid, Resource<PciDeviceHandleKind>), ()>),
    /// Hot-remove a dynamically added VPCI device by instance ID.
    RemoveVpciDevice(FailableRpc<Guid, ()>),
    /// Dump VM state (VP registers + memory) to a `.vmrs` file.
    ///
    /// The worker pauses the VM internally, collects state, and restores
    /// the prior running state afterward. The caller provides an open file
    /// handle to write to (typically a temporary file that gets renamed
    /// into place on success).
    DumpState(FailableRpc<File, ()>),
}

/// State returned after a successful bounded snapshot quiesce.
#[derive(Debug, MeshPayload)]
pub struct SnapshotSaveResponse {
    /// Encoded VM saved state.
    pub saved_state: ProtobufMessage,
    /// Complete state-unit inventory in stable registration order.
    pub state_unit_names: Vec<String>,
    /// Effective Linux direct command line loaded into the guest.
    pub effective_command_line: String,
    /// Effective guest TSC frequency.
    pub tsc_frequency_hz: u64,
    /// Effective local APIC timer frequency.
    pub apic_frequency_hz: u64,
    /// Host wall time at the stopped capture boundary.
    pub capture_wall_clock: mesh::payload::Timestamp,
    /// Canonical effective CPU compatibility contract.
    pub cpu_contract: Vec<u8>,
}

/// Failure classification for a bounded snapshot quiesce/save operation.
#[derive(Debug, MeshPayload, thiserror::Error)]
pub enum SnapshotQuiesceError {
    /// The request was rejected before any state transition began.
    #[error("snapshot quiesce request was rejected")]
    Rejected(#[source] RemoteError),
    /// No uncertain transition occurred; the controller may request rollback.
    #[error("snapshot quiesce failed without uncertain state")]
    RollbackSafe(#[source] RemoteError),
    /// A unit may have partially transitioned; the VM must be terminated.
    #[error("snapshot quiesce left uncertain state")]
    Uncertain(#[source] RemoteError),
}

#[derive(Debug, MeshPayload, thiserror::Error)]
pub enum PulseSaveRestoreError {
    #[error("reset not supported")]
    ResetNotSupported,
    #[error("pulse save+restore failed")]
    Other(#[source] RemoteError),
    #[error("save and restore are unavailable for this machine profile")]
    UnsupportedMachineProfile,
}

impl From<anyhow::Error> for PulseSaveRestoreError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(RemoteError::new(err))
    }
}

impl fmt::Debug for VmRpc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            VmRpc::Reset(_) => "Reset",
            VmRpc::Save(_) => "Save",
            VmRpc::QuiesceForSnapshot(_) => "QuiesceForSnapshot",
            VmRpc::ResumeAfterFailedSnapshot(_) => "ResumeAfterFailedSnapshot",
            VmRpc::ReleaseSnapshotBoundary(_) => "ReleaseSnapshotBoundary",
            VmRpc::Resume(_) => "Resume",
            VmRpc::Pause(_) => "Pause",
            VmRpc::ClearHalt(_) => "ClearHalt",
            VmRpc::Nmi(_) => "Nmi",
            VmRpc::AddVmbusDevice(_) => "AddVmbusDevice",
            VmRpc::ConnectHvsock(_) => "ConnectHvsock",
            VmRpc::PulseSaveRestore(_) => "PulseSaveRestore",
            VmRpc::StartReloadIgvm(_) => "StartReloadIgvm",
            VmRpc::CompleteReloadIgvm(_) => "CompleteReloadIgvm",
            VmRpc::ReadMemory(_) => "ReadMemory",
            VmRpc::WriteMemory(_) => "WriteMemory",
            VmRpc::UpdateCliParams(_) => "UpdateCliParams",
            VmRpc::AddPcieDevice(_) => "AddPcieDevice",
            VmRpc::RemovePcieDevice(_) => "RemovePcieDevice",
            VmRpc::AddVpciDevice(_) => "AddVpciDevice",
            VmRpc::RemoveVpciDevice(_) => "RemoveVpciDevice",
            VmRpc::DumpState(_) => "DumpState",
        };
        f.pad(s)
    }
}
