// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides a safe wrapper around some CPU instructions.
//!
//! This is needed because Rust's intrinsics are marked unsafe (despite
//! these few being completely safe to invoke).

#![no_std]
// UNSAFETY: Calling a cpu intrinsic.
#![expect(unsafe_code)]

/// Invokes the cpuid instruction with input values `eax` and `ecx`.
#[cfg(target_arch = "x86_64")]
pub fn cpuid(eax: u32, ecx: u32) -> core::arch::x86_64::CpuidResult {
    // SAFETY: this instruction is always safe to invoke. If the instruction is
    // for some reason not supported, the process will fault in an OS-specific
    // way, but this will not cause memory safety violations.
    unsafe { core::arch::x86_64::__cpuid_count(eax, ecx) }
}

/// Invokes the rdtsc instruction.
#[cfg(target_arch = "x86_64")]
pub fn rdtsc() -> u64 {
    // SAFETY: The tsc is safe to read.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Emit a store fence to flush the processor's store buffer
pub fn store_fence() {
    cfg_if::cfg_if! {
        if #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: this instruction has no safety requirements.
            unsafe { core::arch::x86_64::_mm_sfence() }
        }
        else if #[cfg(target_arch = "aarch64")]
        {
            // SAFETY: this instruction has no safety requirements.
            unsafe { core::arch::asm!("dsb st", options(nostack)) };
        }
        else
        {
            compile_error!("Unsupported architecture");
        }
    }

    // Make the compiler aware.
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::Release);
}
