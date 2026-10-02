// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for the protocol between `tmk_vmm` and the test microkernel.

#![no_std]
#![forbid(unsafe_code)]

use bitfield_struct::bitfield;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
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

/// Test metadata flags.
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct TestFlags64 {
    #[bits(1)]
    pub expected_failure: bool,
    #[bits(1)]
    pub linux_only: bool,
    #[bits(1)]
    pub time_control: bool,
    #[bits(1)]
    pub tsc_deadline: bool,
    #[bits(60)]
    reserved: u64,
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
    /// Test metadata flags.
    pub flags: TestFlags64,
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
    /// Stop the VP and perform a host-controlled timekeeping operation.
    TimeCheckpoint(TimeCheckpoint),
}

/// Operation performed after the command's MMIO write has completed.
#[repr(u32)]
#[derive(Debug, Copy, Clone, TryFromBytes)]
pub enum TimeAction {
    /// Measure TSC rate while time is running and the VP is stopped.
    Calibrate,
    /// Freeze, wait, and thaw without rewriting saved state.
    Pause,
    /// Serialize, reset, and restore state while frozen.
    SaveRestore,
    /// Restore serialized state into a newly created partition.
    Recreate,
}

/// A guest/host rendezvous for timekeeping tests.
#[repr(C)]
#[derive(Debug, Copy, Clone, TryFromBytes)]
pub struct TimeCheckpoint {
    /// Operation requested by the guest.
    pub action: TimeAction,
    /// Optional unrelated interrupt vector to inject while frozen (zero: none).
    pub interrupt_vector: u32,
    /// Guest address of a writable [`TimeCheckpointResult`].
    pub result_gpa: u64,
}

/// Host observations at a timekeeping rendezvous.
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, FromBytes, IntoBytes, Immutable, zerocopy::KnownLayout)]
pub struct TimeCheckpointResult {
    /// Guest TSC when the VP stopped (after freezing, except for calibration).
    pub tsc_before: u64,
    /// Guest TSC immediately before execution is allowed again.
    pub tsc_after: u64,
    /// Measured host time spent waiting, in nanoseconds.
    pub elapsed_ns: u64,
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
