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

/// AArch64 CPU ID and feature registers.
#[cfg(target_arch = "aarch64")]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Aarch64CpuidRegister {
    /// Multiprocessor affinity register.
    MpidrEl1,
    /// Main ID register.
    MidrEl1,
    /// Revision ID register.
    RevidrEl1,
    /// AArch64 processor feature register 0.
    IdAa64Pfr0El1,
    /// AArch64 processor feature register 1.
    IdAa64Pfr1El1,
    /// AArch64 debug feature register 0.
    IdAa64Dfr0El1,
    /// AArch64 debug feature register 1.
    IdAa64Dfr1El1,
    /// AArch64 instruction set attribute register 0.
    IdAa64Isar0El1,
    /// AArch64 instruction set attribute register 1.
    IdAa64Isar1El1,
    /// AArch64 memory model feature register 0.
    IdAa64Mmfr0El1,
    /// AArch64 memory model feature register 1.
    IdAa64Mmfr1El1,
    /// AArch64 memory model feature register 2.
    IdAa64Mmfr2El1,
}

/// Reads an AArch64 CPU ID or feature register.
#[inline]
#[cfg(target_arch = "aarch64")]
pub fn cpuid(register: Aarch64CpuidRegister) -> u64 {
    let value: u64;

    // SAFETY: Reading these ID registers has no memory safety requirements.
    unsafe {
        match register {
            Aarch64CpuidRegister::MpidrEl1 => {
                core::arch::asm!("mrs {value}, MPIDR_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
            Aarch64CpuidRegister::MidrEl1 => {
                core::arch::asm!("mrs {value}, MIDR_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
            Aarch64CpuidRegister::RevidrEl1 => {
                core::arch::asm!("mrs {value}, REVIDR_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
            Aarch64CpuidRegister::IdAa64Pfr0El1 => {
                core::arch::asm!("mrs {value}, ID_AA64PFR0_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
            Aarch64CpuidRegister::IdAa64Pfr1El1 => {
                core::arch::asm!("mrs {value}, ID_AA64PFR1_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
            Aarch64CpuidRegister::IdAa64Dfr0El1 => {
                core::arch::asm!("mrs {value}, ID_AA64DFR0_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
            Aarch64CpuidRegister::IdAa64Dfr1El1 => {
                core::arch::asm!("mrs {value}, ID_AA64DFR1_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
            Aarch64CpuidRegister::IdAa64Isar0El1 => {
                core::arch::asm!("mrs {value}, ID_AA64ISAR0_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
            Aarch64CpuidRegister::IdAa64Isar1El1 => {
                core::arch::asm!("mrs {value}, ID_AA64ISAR1_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
            Aarch64CpuidRegister::IdAa64Mmfr0El1 => {
                core::arch::asm!("mrs {value}, ID_AA64MMFR0_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
            Aarch64CpuidRegister::IdAa64Mmfr1El1 => {
                core::arch::asm!("mrs {value}, ID_AA64MMFR1_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
            Aarch64CpuidRegister::IdAa64Mmfr2El1 => {
                core::arch::asm!("mrs {value}, ID_AA64MMFR2_EL1", value = out(reg) value, options(nomem, nostack, preserves_flags));
            }
        }
    }

    value
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
