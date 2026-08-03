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

/// Fixed AArch64 platform configuration used by `tmk_vmm` and AArch64 TMKs.
pub mod aarch64 {
    /// Guest MMIO base address of the GICv3 distributor.
    ///
    /// The distributor occupies a 64-KiB frame starting at `0xff00_0000`.
    pub const GIC_DISTRIBUTOR_BASE: u64 = 0xff00_0000;
    /// Guest MMIO base address of the first GICv3 redistributor.
    ///
    /// Each VP uses a 128-KiB redistributor region containing one 64-KiB
    /// redistributor frame and one 64-KiB SGI/PPI frame.
    pub const GIC_REDISTRIBUTOR_BASE: u64 = 0xff02_0000;
    /// Architectural virtual timer PPI.
    pub const VIRTUAL_TIMER_PPI: u32 = 20;
    /// Total number of GIC interrupts exposed by the test platform.
    ///
    /// This provides SGIs 0-15, PPIs 16-31, and SPIs 32-255. Architectural
    /// INTIDs 256-1019 are not implemented by this test platform.
    pub const GIC_INTERRUPT_COUNT: u32 = 256;
}

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
    #[bits(62)]
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
