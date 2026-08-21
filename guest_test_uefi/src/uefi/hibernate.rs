// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Requests a platform hibernation from the guest.

// UNSAFETY: Raw port I/O / SMC needed to request an ACPI/PSCI power transition.
#![expect(unsafe_code)]

/// Request hibernation from the platform and do not return.
///
/// On x86_64 this writes the ACPI PM control register requesting S4; on aarch64
/// it issues the PSCI `SYSTEM_OFF2` call requesting hibernation. Under OpenHCL
/// these are trapped by the paravisor, which records the hibernate token and
/// notifies the host.
pub fn hibernate() -> ! {
    uefi::println!("guest_test_uefi: requesting hibernation");

    #[cfg(target_arch = "x86_64")]
    // SAFETY: Writing the emulated ACPI PM control register (PM base 0x400 +
    // control offset 0x04) with SLP_EN set and suspend type 1 requests S4
    // (hibernate). The write has no memory effects on the guest.
    unsafe {
        core::arch::asm!(
            "out dx, ax",
            in("dx") 0x404u16,
            in("ax") 0x2400u16,
            options(nomem, nostack, preserves_flags),
        );
    }

    #[cfg(target_arch = "aarch64")]
    // SAFETY: PSCI SYSTEM_OFF2 (0x8400_0015) with HIBERNATE_OFF (1) requests
    // hibernation and does not return. x0/x1 are marked clobbered (and flags are
    // not preserved) because an SMC may modify them if it unexpectedly returns.
    unsafe {
        core::arch::asm!(
            "smc #0",
            inlateout("x0") 0x8400_0015u64 => _,
            inlateout("x1") 1u64 => _,
            options(nomem, nostack),
        );
    }

    // The platform should have halted this VP; if control returns, spin so we
    // never fall through to unrelated code.
    loop {
        core::hint::spin_loop();
    }
}
