// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generic boot-action selector for `guest_test_uefi`.
//!
//! Petri selects the guest's behavior by seeding a UEFI variable (via a
//! `CUSTOM_UEFI` NVRAM delta in the VMGS) that this application reads at
//! startup. When the variable is absent or unrecognized, the default action is
//! to run the normal test suite; specific values select alternate behaviors
//! such as requesting hibernation.

// UNSAFETY: Raw port I/O / SMC needed to request an ACPI/PSCI power transition.
#![expect(unsafe_code)]

use uefi::cstr16;
use uefi::runtime;
use uefi::runtime::VariableVendor;

/// The UEFI variable, under the EFI global namespace, that petri writes to
/// select a guest action. Keep in sync with the petri-side seeding helper.
const ACTION_VAR_NAME: &uefi::CStr16 = cstr16!("PetriBootAction");

/// The action a caller requested this application to perform at startup.
pub enum GuestAction {
    /// Run the normal `guest_test_uefi` test suite (the default).
    RunTests,
    /// Request hibernation from the platform, then never return.
    Hibernate,
}

impl GuestAction {
    /// Map a raw variable value to an action, tolerating a trailing NUL.
    fn from_value(value: &[u8]) -> Self {
        let value = value.split(|&b| b == 0).next().unwrap_or(value);
        match value {
            b"hibernate" => GuestAction::Hibernate,
            _ => GuestAction::RunTests,
        }
    }
}

/// Read the petri-selected action, defaulting to running the test suite when
/// the selector variable is absent or unreadable.
pub fn selected_action() -> GuestAction {
    match runtime::get_variable_boxed(ACTION_VAR_NAME, &VariableVendor::GLOBAL_VARIABLE) {
        Ok((value, _)) => GuestAction::from_value(&value),
        Err(_) => GuestAction::RunTests,
    }
}

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
    // hibernation and does not return.
    unsafe {
        core::arch::asm!(
            "smc #0",
            in("x0") 0x8400_0015u64,
            in("x1") 1u64,
            options(nomem, nostack, preserves_flags),
        );
    }

    // The platform should have halted this VP; if control returns, spin so we
    // never fall through to unrelated code.
    loop {
        core::hint::spin_loop();
    }
}
