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
    core::arch::x86_64::__cpuid_count(eax, ecx)
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

/// Read the CNTFRQ_EL0 system register, which contains the frequency of the
/// system timer in Hz. This is used to determine the frequency of the
/// system timer for the current execution level (EL0).
#[inline]
#[cfg(target_arch = "aarch64")]
pub fn read_cntfrq_el0() -> u64 {
    let freq: u64;
    // SAFETY: no safety requirements, just reading an EL0 sysreg
    unsafe {
        core::arch::asm!(
            "mrs {cntfrq}, cntfrq_el0",
            cntfrq = out(reg) freq,
            options(nomem, nostack, preserves_flags)
        );
    };
    freq
}
