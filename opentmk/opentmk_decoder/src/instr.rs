use alloc::vec::Vec;

use crate::wire;

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct InstrCopyIn {
    /// The on-the-wire instruction.
    pub(crate) wire: wire::InstrCopyIn,
    /// The PID of the program.
    pub(crate) rpid: u64,
    /// The data source.
    pub(crate) arg: Arg,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum Arg {
    Const(wire::ArgConst),
    Result(wire::ArgResult),
    Data((wire::ArgData, Vec<u64>)),
    // CSum(),
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub(crate) struct InstrCopyOut {
    /// The on-the-wire instruction.
    pub(crate) wire: wire::InstrCopyOut,
    /// The PID of the program.
    pub(crate) rpid: u64,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct InstrCall {
    /// The on-the-wire instruction.
    pub(crate) wire: wire::InstrCall,
    /// The PID of the program.
    pub(crate) rpid: u64,
    /// The index of the function to call.
    pub(crate) idx: i64,
    /// Custom copyout index, *only* set if the wire::InstrCall::copyout_index is COPYOUT_INDEX_INVALID.
    /// This allows us to track success/failure of a call that has not been provided a copyout index from syzkaller.
    /// In this case, these indexes index into our custom results array (not the default results array).
    pub(crate) custom_copyout_index: Option<usize>,
    /// The arguments.
    pub(crate) args: Vec<Arg>,
}

/// A decoded syzkaller instruction.
pub(crate) enum InstrEntry {
    /// A call instruction with a list of associated copyout instructions.
    Call(InstrCall, Vec<InstrCopyOut>),
    /// A copyin instruction.
    CopyIn(InstrCopyIn),
}

impl InstrEntry {
    pub(crate) fn get_inner_instr(&self) -> Instr {
        match self {
            InstrEntry::Call(i, _) => Instr::Call(i.clone()),
            InstrEntry::CopyIn(i) => Instr::CopyIn(i.clone()),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum Instr {
    /// Copy data into memory.
    CopyIn(InstrCopyIn),
    /// Copy data out of memory into the results array.
    /// These instructions usually follow a call instruction.
    CopyOut(InstrCopyOut),
    /// Call a system call.
    Call(InstrCall),
}
