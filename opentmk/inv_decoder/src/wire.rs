// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structures representing on-the-wire data formats.
use super::*;

pub const INSTR_EOF: i64 = -1;
pub const INSTR_COPYIN: i64 = -2;
pub const INSTR_COPYOUT: i64 = -3;

pub const ARG_CONST: u64 = 0;
pub const ARG_RESULT: u64 = 1;
pub const ARG_DATA: u64 = 2;

/// usize::MAX (aka -1) as a copyout index value
/// indicates we should skip any copying out of the call's result
pub const COPYOUT_INDEX_INVALID: usize = usize::MAX;

/// The header of a syzkaller program.
#[derive(IntoBytes, FromBytes, Immutable, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct ProgramHeader {
    pub magic: u64,
    /// Environment flags bitfield:
    /// 0: debug
    /// 1: coverage
    /// 2: sandbox_setuid
    /// 3: sandbox_namespace
    /// 4: sandbox_android_untrusted_app
    /// 5: enable_tun
    /// 6: enable_net_dev
    /// 7: enable_fault_injection
    pub env_flags: u64,
    /// Executor flags bitfield:
    /// 0: collect_cover
    /// 1: dedup_cover
    /// 2: inject_fault
    /// 3: collect_comps
    /// 4: threaded
    /// 5: collide
    pub exec_flags: u64,
    /// Process ID
    pub pid: u64,
    /// Inject a fault in the corresponding index in the call table
    pub fault_call: u64,
    /// Inject a fault in the nth call in the program
    pub fault_nth: u64,
    pub prog_size: u64,
}

/// Copyin instruction
#[derive(IntoBytes, FromBytes, Immutable, Debug, PartialEq, Eq, Copy, Clone)]
#[repr(C)]
pub struct InstrCopyIn {
    pub addr: u64,
    // Followed by an argument (Arg*).
}

/// Copyout instruction
#[derive(IntoBytes, FromBytes, Immutable, Debug, PartialEq, Eq, Copy, Clone)]
#[repr(C)]
pub struct InstrCopyOut {
    /// The index into the results array.
    pub index: u64,
    /// The address of the memory to copy from.
    pub addr: u64,
    /// The size to copy, in bytes.
    pub size: u64,
}

/// Call instruction
#[derive(IntoBytes, FromBytes, Immutable, Debug, PartialEq, Eq, Copy, Clone)]
#[repr(C)]
pub struct InstrCall {
    pub copyout_index: u64,
    pub num_args: u64,
    // Arguments array follows...
}

/// Constant argument (ty = 0)
#[derive(IntoBytes, FromBytes, Immutable, Debug, PartialEq, Eq, Copy, Clone)]
#[repr(C)]
pub struct ArgConst {
    /// Packed bitfield describing the constant argument.
    pub meta: u64,
    /// The value of the argument.
    pub val: u64,
}

/// Result argument (ty = 1)
#[derive(IntoBytes, FromBytes, Immutable, Debug, PartialEq, Eq, Copy, Clone)]
#[repr(C)]
pub struct ArgResult {
    /// Packed bitfield describing the result argument.
    pub meta: u64,
    /// The index of the result argument.
    pub idx: u64,
    /// A value to divide the result by.
    pub op_div: u64,
    /// A value to add to the result.
    pub op_add: u64,
    /// The value to use if the result at `idx` was not initialized.
    pub arg: u64,
}

/// Data argument (ty = 2)
#[derive(IntoBytes, FromBytes, Immutable, Debug, PartialEq, Eq, Copy, Clone)]
#[repr(C)]
pub struct ArgData {
    /// Data size, in bytes.
    pub size: u64,
    // Data words follow...
}
