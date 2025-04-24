// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for the protocol between `tmk_vmm` and the test microkernel.

#![no_std]

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::TryFromBytes;

/// Start input from the VMM to the TMK.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable)]
pub struct StartInput {
    /// The address to write commands to.
    pub command: u64,
    /// The test index.
    pub test_index: u64,
}

/// A 64-bit TMK test descriptor.
#[repr(C)]
#[derive(IntoBytes, FromBytes, Immutable)]
pub struct TestDescriptor64 {
    /// The address of the test's name.
    pub name: u64,
    /// The length of the test's name.
    pub name_len: u64,
    /// The test entry point.
    pub entrypoint: u64,
}

/// TMK command.
#[repr(u32)]
#[derive(TryFromBytes)]
pub enum Command {
    /// Log a UTF-8 message string.
    Log(StrDescriptor),
    /// The test panicked.
    Panic {
        /// The panic message.
        message: StrDescriptor,
        /// The file and line where the panic occurred.
        filename: StrDescriptor,
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
pub struct StrDescriptor {
    /// Pointer to the string.
    pub gpa: u64,
    /// Length of the string.
    pub len: u64,
}
