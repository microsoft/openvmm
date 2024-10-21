// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides a safe wrapper around some x86-64 instructions.
//!
//! This is needed because Rust's intrinsics are marked unsafe (despite
//! these few being completely safe to invoke).

#![no_std]
#![cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch cpu-intrinsic
// UNSAFETY: Calling a cpu intrinsic.
#![allow(unsafe_code)]
use core::arch::x86_64::CpuidResult;

/// Invokes the cpuid instruction with input values `eax` and `ecx`.
pub fn cpuid(eax: u32, ecx: u32) -> CpuidResult {
    // SAFETY: this instruction is always safe to invoke. If the instruction is
    // for some reason not supported, the process will fault in an OS-specific
    // way, but this will not cause memory safety violations.
    unsafe { core::arch::x86_64::__cpuid_count(eax, ecx) }
}

/// Invokes the rdtsc instruction.
pub fn rdtsc() -> u64 {
    // SAFETY: The tsc is safe to read.
    unsafe { core::arch::x86_64::_rdtsc() }
}
