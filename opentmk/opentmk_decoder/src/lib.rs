// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Decodes and executes syzkaller programs against a caller-provided memory map.

#![no_std]

#[macro_use]
extern crate alloc;

mod decoder;
mod instr;
mod prog;
mod safememory;
mod wire;

pub use decoder::DecoderError;
use prog::DecodedProgram;
pub use prog::InputCase;
pub use prog::InputResult;
pub use prog::TestcaseResults;
pub use safememory::SafeMemoryMap;
pub use safememory::SingleMap;

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

const K_MAX_COMMANDS: usize = 1000;

/// Required size, in bytes, of the syzkaller executor memory region.
pub const EXEC_INPUT_REQ_SIZE: usize = 0x1000000;

/// Supported (min) input size (kMaxInput in executor.cc).
pub const SUPPORTED_INPUT_SIZE: usize = 8 << 20;

/// Max supported args
pub const MAX_ARGS: usize = 30;

/// All syzkaller pointers are offsets from this presumed base address
pub const ADDR_SYZ_BEGIN: u64 = 0x20000000;

/// Call failed
const _SYZKALLER_CALL_END_FAILED: u64 = 3;

/// Takes a syz_in buffer containing raw syzkaller data from TKO, parses
/// individual test cases from it into the provided addr buffer, and calls the
/// provided exec function with a single test case; then continues from the
/// beginning until all provided testcases have completed.
///
/// It is expected that addr_size is minimum 0x1000000 bytes (or 16MiB).
/// syz_exec_mem must be at least [`EXEC_INPUT_REQ_SIZE`] in size.
/// syz_input_buffer must be at least [`SUPPORTED_INPUT_SIZE`] in size.
pub fn exec_testcases_safe<M: SafeMemoryMap, F>(
    syz_exec_mem: M,
    syz_input_buffer: &mut [u8],
    exec: F,
) -> Result<TestcaseResults, DecoderError>
where
    F: Fn(&mut M, InputCase) -> InputResult + Send + Sync,
{
    let mut decoded = DecodedProgram::new(syz_exec_mem, syz_input_buffer, exec)?;
    decoded.exec_instrs()?;
    Ok(decoded.results.as_ref().clone())
}
