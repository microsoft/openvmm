// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The message definitions used to process debugging requests from
//! `debug_worker`.

pub use virt::x86::BreakpointSize;
pub use virt::x86::BreakpointType;
pub use virt::x86::DebugState;
pub use virt::x86::HardwareBreakpoint;
pub use virt::x86::VpState;

use mesh::rpc::FailableRpc;
use mesh::MeshPayload;

#[derive(Debug, MeshPayload)]
pub enum DebugRequest {
    /// The debugger has been attached.
    Attach,
    /// The debugger has been detached.
    Detach,
    /// Resume the VM, responding with a [`DebugStopReason`] once the VM encounters a stop condition (e.g: breakpoint hit)
    Resume {
        response: mesh::OneshotSender<DebugStopReason>,
    },
    /// Debugger is requesting a manual break.
    Break,
    /// Sets the hardware debugger state for a VP.
    SetDebugState { vp: u32, state: DebugState },
    /// Fetch the specified vp's register state.
    GetVpState(FailableRpc<u32, Box<VpState>>),
    /// Set the specified vp's register state.
    SetVpState(FailableRpc<(u32, Box<VpState>), ()>),
    /// Read from the specified GPA from the guest.
    ReadMemory(FailableRpc<(GuestAddress, usize), Vec<u8>>),
    /// Write to the specified GPA from the guest.
    WriteMemory(FailableRpc<(GuestAddress, Vec<u8>), ()>),
}

/// An address within the Guest
#[derive(Debug, MeshPayload)]
pub enum GuestAddress {
    /// Guest Virtual Address
    Gva { vp: u32, gva: u64 },
    /// Guest Physical Address
    Gpa(u64),
}

#[derive(MeshPayload, Debug)]
pub enum DebugStopReason {
    /// Break has been acknowledged + executed.
    Break,
    /// VM has powered off.
    PowerOff,
    /// VM has been reset.
    Reset,
    /// `vp` has encountered a triple fault.
    TripleFault { vp: u32 },
    /// `vp` has completed a single step.
    SingleStep { vp: u32 },
    /// `vp` has reached a hardware breakpoint.
    HwBreakpoint {
        vp: u32,
        breakpoint: HardwareBreakpoint,
    },
}
