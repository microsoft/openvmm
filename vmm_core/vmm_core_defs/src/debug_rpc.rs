// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The message definitions used to process debugging requests from
//! `debug_worker`.

use mesh::payload::Protobuf;
pub use virt::x86::BreakpointSize;
pub use virt::x86::BreakpointType;
pub use virt::x86::DebugState;
pub use virt::x86::HardwareBreakpoint;

use mesh::MeshPayload;
use mesh::rpc::FailableRpc;
use virt::x86::SegmentRegister;

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
    GetVpState(FailableRpc<u32, Box<DebuggerVpState>>),
    /// Set the specified vp's register state.
    SetVpState(FailableRpc<(u32, Box<DebuggerVpState>), ()>),
    /// Read from the specified GPA from the guest.
    ReadMemory(FailableRpc<(GuestAddress, usize), Vec<u8>>),
    /// Write to the specified GPA from the guest.
    WriteMemory(FailableRpc<(GuestAddress, Vec<u8>), ()>),
}

/// Register state for a VP.
///
/// This has all the supported architectures embedded in it to avoid having
/// arch-specific compilation at this layer.
#[derive(Debug, Protobuf)]
pub enum DebuggerVpState {
    X86_64(X86VpState),
    Aarch64(Aarch64VpState),
}

/// Subset of VP state for debuggers.
#[derive(Debug, PartialEq, Eq, Protobuf)]
pub struct X86VpState {
    pub gp: [u64; 16],
    pub rip: u64,
    pub rflags: u64,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
    pub kernel_gs_base: u64,
    pub es: SegmentRegister,
    pub cs: SegmentRegister,
    pub ss: SegmentRegister,
    pub ds: SegmentRegister,
    pub fs: SegmentRegister,
    pub gs: SegmentRegister,
}

#[derive(Debug, PartialEq, Eq, Protobuf)]
pub struct Aarch64VpState {
    pub x: [u64; 31],
    pub sp_el0: u64,
    pub sp_el1: u64,
    pub pc: u64,
    pub cpsr: u64,
    pub sctlr_el1: u64,
    pub tcr_el1: u64,
    pub ttbr0_el1: u64,
    pub ttbr1_el1: u64,
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
