// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! aarch64 specific tests.

#![cfg(target_arch = "aarch64")]
#![allow(
    unsafe_code,
    reason = "naked functions required for AArch64 trampoline code"
)]

use crate::prelude::*;
use tmk_protocol as _;

/// # Safety
/// The test harness must expect an instruction abort outside the protected address range.
// SAFETY: Assembly only uses caller-saved X16 to branch to the intended fault
// address and never returns; the calling test expects the instruction abort.
#[unsafe(naked)]
unsafe extern "C" fn instruction_abort_outside_par_entry() -> ! {
    core::arch::naked_asm!(
        "movz x16, #0x0000",
        "movk x16, #0x0000, lsl #16",
        "movk x16, #0xffff, lsl #32",
        "movk x16, #0x0000, lsl #48",
        "br x16",
    );
}

#[tmk_test(expected_failure, linux_only)]
fn instruction_abort_outside_par(_: TestContext<'_>) {
    log!("instruction_abort_outside_par");

    // SAFETY: This test intentionally jumps to an assembly entry point that
    // triggers an instruction abort and never returns.
    unsafe {
        instruction_abort_outside_par_entry();
    }
}

/// # Safety
/// The test harness must expect an instruction abort at a RIPAS-empty address.
// SAFETY: Assembly only uses caller-saved X16 to branch to the intended fault
// address and never returns; the calling test expects the instruction abort.
#[unsafe(naked)]
unsafe extern "C" fn instruction_abort_ripas_empty_entry() -> ! {
    core::arch::naked_asm!("movz x16, #0x0000", "br x16");
}

#[tmk_test(expected_failure, linux_only)]
fn instruction_abort_ripas_empty(_: TestContext<'_>) {
    log!("instruction_abort_ripas_empty");

    // SAFETY: This test intentionally transfers control to an assembly entry
    // point that executes from an address chosen to provoke the expected
    // instruction abort. The entry point is defined above and never returns.
    unsafe {
        instruction_abort_ripas_empty_entry();
    }
}

/// # Safety
/// The test harness must expect an instruction abort caused by execute permissions.
// SAFETY: Assembly only uses caller-saved X16 to branch to the intended fault
// address and never returns; the calling test expects the instruction abort.
#[unsafe(naked)]
unsafe extern "C" fn instruction_abort_permissions_enabled_entry() -> ! {
    core::arch::naked_asm!("movz x16, #0xf000", "movk x16, #0x847f, lsl #16", "br x16",);
}

#[tmk_test(expected_failure, linux_only)]
fn instruction_abort_permissions_enabled(_: TestContext<'_>) {
    log!("instruction_abort_permissions_enabled");

    // SAFETY: This test intentionally calls an assembly entry point that jumps
    // to an address expected to fault under the configured permissions. The
    // entry point is defined in this module and is declared `-> !`.
    unsafe {
        instruction_abort_permissions_enabled_entry();
    }
}
