// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client definitions for the gdbstub debug worker.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use mesh::MeshPayload;
use mesh::payload::Protobuf;
use mesh_worker::WorkerId;
use std::net::TcpListener;
use vmm_core_defs::debug_rpc::DebugRequest;

#[derive(MeshPayload)]
pub struct DebuggerParameters<T> {
    pub listener: T,
    pub req_chan: mesh::Sender<DebugRequest>,
    pub vp_count: u32,
    pub target_arch: TargetArch,
}

#[derive(Debug, Copy, Clone, Protobuf)]
pub enum TargetArch {
    X86_64,
    I8086,
    Aarch64,
}

pub const DEBUGGER_WORKER: WorkerId<DebuggerParameters<TcpListener>> =
    WorkerId::new("DebuggerWorker");

#[cfg(any(windows, target_os = "linux"))]
pub const DEBUGGER_VSOCK_WORKER: WorkerId<DebuggerParameters<vmsocket::VmListener>> =
    WorkerId::new("DebuggerVsockWorker");
