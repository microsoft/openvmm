// Copyright (C) Microsoft Corporation. All rights reserved.

//! Client definitions for the gdbstub debug worker.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use mesh_worker::WorkerId;
use std::net::TcpListener;
use vmm_core_defs::debug_rpc::DebugRequest;

#[derive(MeshPayload)]
pub struct DebuggerParameters<T> {
    pub listener: T,
    pub req_chan: mesh::Sender<DebugRequest>,
    pub vp_count: u32,
}

pub const DEBUGGER_WORKER: WorkerId<DebuggerParameters<TcpListener>> =
    WorkerId::new("DebuggerWorker");

#[cfg(any(windows, target_os = "linux"))]
pub const DEBUGGER_VSOCK_WORKER: WorkerId<DebuggerParameters<vmsocket::VmListener>> =
    WorkerId::new("DebuggerVsockWorker");