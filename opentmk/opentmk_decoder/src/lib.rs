// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Decodes and executes syzkaller programs against a caller-provided memory map.
//!
//! # Encoded program format
//!
//! This crate consumes the fixed-width executor encoding used by the syzkaller
//! integration for OpenTMK. The decoding flow corresponds to the instruction
//! loop and argument readers in syzkaller's
//! [`executor.cc`](https://github.com/google/syzkaller/blob/master/executor/executor.cc),
//! but the upstream format evolves independently.
//!
//! All scalar fields are unsigned 64-bit little-endian words. Negative
//! instruction values are represented in two's complement. The input starts
//! with this 56-byte header:
//!
//! | Byte offset | Field | Meaning |
//! | ---: | --- | --- |
//! | 0 | `magic` | Must be `0xbadc0ffeebadface`. |
//! | 8 | `env_flags` | Environment flags; see below. |
//! | 16 | `exec_flags` | Executor flags; see below. |
//! | 24 | `pid` | Executor process identifier used by constant arguments. |
//! | 32 | `fault_call` | Call index selected for fault injection. |
//! | 40 | `fault_nth` | Invocation of `fault_call` selected for fault injection. |
//! | 48 | `prog_size` | Size in bytes of the instruction stream that follows. |
//!
//! The decoder currently uses `pid` and `prog_size`, validates `magic`, and
//! ignores the flag and fault-injection fields. In `env_flags`, bits 0 through
//! 7 respectively mean debug, coverage, setuid sandbox, namespace sandbox,
//! Android untrusted-app sandbox, TUN, network devices, and fault injection.
//! In `exec_flags`, bits 0 through 5 respectively mean collect coverage,
//! deduplicate coverage, inject fault, collect comparisons, threaded
//! execution, and collide calls.
//!
//! ## Instructions
//!
//! The instruction stream is a sequence of records beginning with an
//! instruction word:
//!
//! | Instruction word | Remaining words | Meaning |
//! | --- | --- | --- |
//! | `u64::MAX` (`-1`) | none | End of the program. |
//! | `u64::MAX - 1` (`-2`) | `addr`, argument | Copy the decoded argument to `addr`. |
//! | `u64::MAX - 2` (`-3`) | `index`, `addr`, `size` | After the preceding call succeeds, copy `size` bytes from `addr` into result `index`. |
//! | `0..=i64::MAX` | `copyout_index`, `num_args`, arguments | Invoke the function identified by the instruction word. |
//!
//! A `copyout_index` of `u64::MAX` means the call has no syzkaller-visible
//! result slot. At most [`MAX_ARGS`] arguments are passed to the execution
//! callback. Copyout instructions are associated with the call immediately
//! before them and are executed only when that call succeeds.
//!
//! ## Arguments
//!
//! Each argument starts with a type word:
//!
//! | Type | Remaining words | Meaning |
//! | ---: | --- | --- |
//! | 0 | `meta`, `value` | Constant value. |
//! | 1 | `meta`, `index`, `op_div`, `op_add`, `fallback` | Reference to an earlier result. |
//! | 2 | `size`, `data...` | Inline data, padded to `ceil(size / 8)` words. |
//!
//! Inline data is supported only as the source of a copy-in instruction.
//! Checksum arguments are reserved by the format but are not implemented by
//! this crate.
//!
//! The `meta` word packs constant-copying information:
//!
//! | Bits | Field |
//! | --- | --- |
//! | 0..=7 | Value size in bytes. |
//! | 8..=15 | Binary format: 0 native, 1 big-endian, 2 decimal string, 3 hexadecimal string, or 4 octal string. |
//! | 16..=23 | Destination bitfield offset. |
//! | 24..=31 | Destination bitfield length. |
//! | 32..=63 | PID stride. The effective constant is `value + pid_stride * pid`. |
//!
//! For a result argument, `index` selects an earlier result. If it is
//! available, its value is divided by `op_div` when that value is nonzero and
//! then incremented by `op_add`; otherwise copy-in uses `fallback`. Direct
//! calls whose result arguments are unavailable are skipped.
//!
//! Addresses refer to the executor memory supplied through [`SafeMemoryMap`].
//! The standard mapping is [`EXEC_INPUT_REQ_SIZE`] bytes beginning at
//! [`ADDR_SYZ_BEGIN`].

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
