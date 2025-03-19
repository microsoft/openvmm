// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Aarch64 entry point and support.

#![cfg(target_arch = "aarch64")]

use super::Str;
use core::arch::global_asm;

static HELLO_WORLD: Str<'static> = Str::new("hello world");

global_asm! {
    ".weak _DYNAMIC",
    ".hidden _DYNAMIC",
    ".globl _start",
    "_start:",
    "adrp x1, {stack}",
    "add x1, x1, :lo12:{stack}",
    "add x1, x1, {STACK_SIZE}",
    "mov sp, x1",

    // Enable the FPU.
    "mrs     x0, CPACR_EL1",
    "orr     x0, x0, #(3 << 20)",
    "orr     x0, x0, #(3 << 16)",
    "msr     CPACR_EL1, x0",
    "isb",

    "adrp x0, __ehdr_start",
    "add x0, x0, :lo12:__ehdr_start",
    "mov x1, x0",
    "adrp x2, _DYNAMIC",
    "add x2, x2, :lo12:_DYNAMIC",
    "bl {relocate}",

    "ldr x0, ={TMK_ADDRESS_LOG}",
    "adrp x1, {hello_world}",
    "add x1, x1, :lo12:{hello_world}",
    "str x1, [x0]",

    "ldr x0, ={TMK_ADDRESS_COMPLETE}",
    "mov x1, 0",
    "str x1, [x0]",
    "1: wfi",
    "b 1b",
    hello_world = sym HELLO_WORLD,
    relocate = sym minimal_rt::reloc::relocate,
    stack = sym STACK,
    STACK_SIZE = const STACK_SIZE,
    TMK_ADDRESS_LOG = const tmk_protocol::TMK_ADDRESS_LOG,
    TMK_ADDRESS_COMPLETE = const tmk_protocol::TMK_ADDRESS_COMPLETE,
}

const STACK_SIZE: usize = 16384;
#[repr(C, align(16))]
struct Stack([u8; STACK_SIZE]);
#[unsafe(no_mangle)]
static mut STACK: Stack = Stack([0; STACK_SIZE]);
