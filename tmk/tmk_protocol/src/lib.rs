// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for the protocol between `tmk_vmm` and the test microkernel.

#![no_std]

use zerocopy::FromBytes;
use zerocopy::TryFromBytes;

/// Address for issuing a command. Write a `&Command` to log.
pub const COMMAND_ADDRESS: u64 = 0xffff0000;

/// TMK command.
#[repr(u32)]
#[derive(TryFromBytes)]
pub enum Command {
    /// Log a UTF-8 message string.
    Log(Str),
    /// The test panicked.
    Panic {
        /// The panic message.
        message: Str,
        /// The file and line where the panic occurred.
        filename: Str,
        /// The line where the panic occurred.
        line: u32,
    },
    /// Complete the test.
    Complete {
        /// Success status of the test.
        success: bool,
    },
}

/// A UTF-8 string in guest memory.
#[repr(C)]
#[derive(FromBytes)]
pub struct Str {
    /// Pointer to the string.
    pub gpa: u64,
    /// Length of the string.
    pub len: u64,
}
