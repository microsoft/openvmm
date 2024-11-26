// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementations for various x86 architectures.

use gdbstub::arch::Arch;
use gdbstub::arch::SingleStepGdbBehavior;

pub mod reg;

/// Implements `Arch` for 64-bit x86 the same way QEMU does
#[expect(non_camel_case_types)]
#[allow(clippy::upper_case_acronyms)]
pub enum X86_64_QEMU {}

impl Arch for X86_64_QEMU {
    type Usize = u64;
    type Registers = reg::X86_64CoreRegs;
    type RegId = reg::id::X86_64CoreRegId;
    type BreakpointKind = usize;

    fn target_description_xml() -> Option<&'static str> {
        // ExdiGdbSrv expects something that looks like QEMU's response, and
        // QEMU uses <xi:include> tags, which cannot be modeled using the base
        // `Arch::target_description_xml` API.
        None
    }

    /// GDB clients unconditionally assume x86 targets support single-stepping.
    fn single_step_gdb_behavior() -> SingleStepGdbBehavior {
        SingleStepGdbBehavior::Required
    }
}

pub enum I8086 {}

impl Arch for I8086 {
    type Usize = u32;
    type Registers = reg::X86CoreRegs;
    type RegId = ();
    type BreakpointKind = usize;

    fn target_description_xml() -> Option<&'static str> {
        Some(include_str!("./i8086.xml").trim())
    }

    /// GDB clients unconditionally assume x86 targets support single-stepping.
    fn single_step_gdb_behavior() -> SingleStepGdbBehavior {
        SingleStepGdbBehavior::Required
    }
}
