// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Running raw assembly/intrinsic.
#![allow(unsafe_code)]

/// Emit a store fence to flush the processor's store buffer.
pub(crate) fn store_fence() {
    // The compiler emits `mfence` on x86_64 and `dmb ...` on aarch64
    // for `std::atomic::fence()` hence the intrinsics and the assembly below.
    #[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch cpu-intrinsic
    {
        // SAFETY: this instruction has no safety requirements.
        unsafe { std::arch::x86_64::_mm_sfence() };
    }
    #[cfg(target_arch = "aarch64")] // xtask-fmt allow-target-arch cpu-intrinsic
    {
        // SAFETY: this instruction has no safety requirements.
        unsafe { std::arch::asm!("dsb st", options(nostack)) };
    }
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        compile_error!("Unsupported architecture");
    }

    // Make the compiler aware.
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::Release);
}
