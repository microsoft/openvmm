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
    /// Ejects a dynamically added PCIe device from the named port.
    ///
    /// Tuple is (cancel_context, port_name, orderly). When orderly is false,
    /// performs surprise removal. Orderly removal requires attention button
    /// support and a running guest that is not halted. It replies after guest
    /// acknowledgement and device teardown.
    /// Timeout or cancellation before removal returns an error without detaching
    /// the endpoint.
    RemovePcieDevice(Rpc<(CancelContext, String, bool), Result<(), RemovePcieDeviceError>>),
    /// Hot-add a VPCI device to VTL0 at runtime with the supplied instance ID.
    AddVpciDevice(FailableRpc<(Guid, Resource<PciDeviceHandleKind>), ()>),
    /// Removes a dynamically added VPCI device by instance ID.
    ///
    /// Tuple is (instance_id, eject_timeout). While running, waits up to the
    /// supplied duration for guest ejection before forcing removal.
    /// VM operations that stop, reset or save the guest wait for pending removal
    /// without shortening the supplied duration.
    RemoveVpciDevice(FailableRpc<(Guid, Duration), ()>),
    /// Dump VM state (VP registers + memory) to a `.vmrs` file.
    ///
    /// The worker pauses the VM internally, collects state, and restores
    /// the prior running state afterward. The caller provides an open file
    /// handle to write to (typically a temporary file that gets renamed
    /// into place on success).
    DumpState(FailableRpc<File, ()>),
}

#[derive(Debug, MeshPayload, thiserror::Error)]
pub enum RemovePcieDeviceError {
    #[error(transparent)]
    Cancelled(#[from] mesh::CancelReason),
    #[error(transparent)]
    Failed(#[from] RemoteError),
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
            VmRpc::AddVpciDevice(_) => "AddVpciDevice",
            VmRpc::RemoveVpciDevice(_) => "RemoveVpciDevice",
            VmRpc::DumpState(_) => "DumpState",
        };
        f.pad(s)
    }
}
