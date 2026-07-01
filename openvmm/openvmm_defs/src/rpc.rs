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
use vm_resource::Resource;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::kind::VmbusDeviceHandleKind;

#[derive(Debug, MeshPayload, Clone, Copy)]
pub enum PcieAerErrorKind {
    Correctable,
    Uncorrectable,
}

#[derive(Debug, MeshPayload, Clone)]
pub struct PcieAerInjectRequest {
    /// Target device Requester ID (Bus<<8 | DevFn) that generated the error.
    ///
    /// The handling port is discovered automatically by walking the topology.
    pub target: u16,
    pub error_kind: PcieAerErrorKind,
    /// Error status bits for COR/UNC status register based on `error_kind`.
    pub status_bits: u32,
    pub header_log: [u32; 4],
}

#[derive(Debug, MeshPayload, Clone)]
pub struct PcieDpcInjectRequest {
    /// Target device Requester ID (Bus<<8 | DevFn) behind the port that should
    /// enter DPC containment.
    ///
    /// The containing port is discovered automatically by walking the topology.
    pub target: u16,
    /// When true, immediately clear RP Busy on the containing port right after
    /// triggering DPC (phase 2 folded into the same call), modeling the Root
    /// Port firmware completing recovery.
    pub complete: bool,
    /// When set, the target device's Uncorrectable Error Status is updated with
    /// these bits (and the handling Root Port records the error message), as if
    /// the uncorrectable error that triggered DPC was reported through AER.
    pub uncorrectable_status_bits: Option<u32>,
    /// AER Header Log recorded on the source device alongside
    /// `uncorrectable_status_bits`.
    pub header_log: [u32; 4],
}

#[derive(MeshPayload)]
pub enum VmRpc {
    Save(FailableRpc<(), ProtobufMessage>),
    Resume(Rpc<(), bool>),
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
    /// Dump VM state (VP registers + memory) to a `.vmrs` file.
    ///
    /// The worker pauses the VM internally, collects state, and restores
    /// the prior running state afterward. The caller provides an open file
    /// handle to write to (typically a temporary file that gets renamed
    /// into place on success).
    DumpState(FailableRpc<File, ()>),
    // NOTE: `MeshPayload` assigns wire field numbers by declaration order, so
    // new variants MUST be appended here (at the end) to avoid changing the
    // numbers of existing variants and breaking wire compatibility.
    /// Inject an AER event at runtime, reported by a target device identified
    /// by its Requester ID (`Bus << 8 | DevFn`). The handling root port is
    /// located automatically by decoding bus ranges; no port name is used.
    InjectPcieAer(FailableRpc<PcieAerInjectRequest, ()>),
    /// Trigger DPC containment at runtime for a target device, identified by
    /// its Requester ID. The containing port is located automatically.
    InjectPcieDpc(FailableRpc<PcieDpcInjectRequest, ()>),
}

#[derive(Debug, MeshPayload, thiserror::Error)]
pub enum PulseSaveRestoreError {
    #[error("reset not supported")]
    ResetNotSupported,
    #[error("pulse save+restore failed")]
    Other(#[source] RemoteError),
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
            VmRpc::InjectPcieAer(_) => "InjectPcieAer",
            VmRpc::InjectPcieDpc(_) => "InjectPcieDpc",
            VmRpc::DumpState(_) => "DumpState",
        };
        f.pad(s)
    }
}
