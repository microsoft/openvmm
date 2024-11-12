// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSR support for boot shim.

use core::arch::asm;

/// Write a value to an MSR.
///
/// # Safety
/// The caller must guarantee that this is a safe operation, based on the
/// behavior of the specified MSR.
#[inline]
pub unsafe fn write_msr(msr: u32, val: u64) {
    let low = val as u32;
    let high = (val >> 32) as u32;

    // SAFETY: Using the `wrmsr` instruction as described in the processor
    // vendors Software Development Manuals.
    unsafe {
        asm!(r#"
        wrmsr
        "#,
        in("eax") low,
        in("edx") high,
        in("ecx") msr);
    }
}

/// Reads a value from an MSR.
///
/// # Safety
/// The caller must guarantee that this is a safe operation, based on the
/// behavior of the specified MSR.
#[inline]
pub unsafe fn read_msr(msr: u32) -> u64 {
    let mut low: u32;
    let mut high: u32;

    // SAFETY: Using the `rdmsr` instruction as described in the processor
    // vendors Software Development Manuals.
    unsafe {
        asm!(r#"
        rdmsr
        "#,
        out("eax") low,
        out("edx") high,
        in("ecx") msr);
    }

    ((high as u64) << 32) | low as u64
}
