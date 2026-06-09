
#![cfg(target_arch = "aarch64")]

#![allow(unsafe_code)]

use crate::prelude::*;

core::arch::global_asm! {
    ".global instruction_abort_outside_par_entry",
    "instruction_abort_outside_par_entry:",
    "movz x16, #0x0000",
    "movk x16, #0x0000, lsl #16",
    "movk x16, #0xffff, lsl #32",
    "movk x16, #0x0000, lsl #48",
    "br x16",
}

unsafe extern "C" {
    fn instruction_abort_outside_par_entry() -> !;
}

#[tmk_test(expected_failure)]
fn instruction_abort_outside_par(_: TestContext<'_>) {
    log!("instruction_abort_outside_par");

    unsafe {
        instruction_abort_outside_par_entry();
    }
}

core::arch::global_asm! {
    ".global instruction_abort_ripas_empty_entry",
    "instruction_abort_ripas_empty_entry:",
    "movz x16, #0x0000",
    "br x16",
}

unsafe extern "C" {
    fn instruction_abort_ripas_empty_entry() -> !;
}

#[tmk_test(expected_failure)]
fn instruction_abort_ripas_empty(_: TestContext<'_>) {
    log!("instruction_abort_ripas_empty");

    unsafe {
        instruction_abort_ripas_empty_entry();
    }
}

core::arch::global_asm! {
    ".global instruction_abort_permissions_enabled_entry",
    "instruction_abort_permissions_enabled_entry:",
    "movz x16, #0xf000",
    "movk x16, #0x847f, lsl #16",
    "br x16",
}

unsafe extern "C" {
    fn instruction_abort_permissions_enabled_entry() -> !;
}

#[tmk_test(expected_failure)]
fn instruction_abort_permissions_enabled(_: TestContext<'_>) {
    log!("instruction_abort_permissions_enabled");

    unsafe {
        instruction_abort_permissions_enabled_entry();
    }
}