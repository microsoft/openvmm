// Copyright (C) Microsoft Corporation. All rights reserved.

//! RPC types for communicating with the VM worker.

use crate::config::DeviceVtl;
use guid::Guid;
use mesh::error::RemoteError;
use mesh::payload::message::ProtobufMessage;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;
use mesh::CancelContext;
use mesh::MeshPayload;
use std::fmt;
use std::fs::File;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_resource::Resource;

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
        };
        f.pad(s)
    }
}
