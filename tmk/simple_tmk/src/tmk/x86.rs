// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64 entry point and support.

#![cfg(target_arch = "x86_64")]

use super::Str;
use core::arch::global_asm;

static HELLO_WORLD: Str<'static> = Str::new("hello world");

global_asm! {
    ".globl _start",
    "_start:",
    "lea rsp, {STACK_SIZE} + {stack}[rip]",
    "lea rdx, _DYNAMIC[rip]",
    "lea rdi, __ehdr_start[rip]",
    "mov rsi, rdi",
    "call {relocate}",
    "mov rcx, {TMK_ADDRESS_LOG}",
    "lea rdx, {hello_world}[rip]",
    "mov qword ptr [rcx], rdx",
    "mov rcx, {TMK_ADDRESS_COMPLETE}",
    "mov byte ptr [rcx], 0",
    "2: hlt",
    "jmp 2b",
    hello_world = sym HELLO_WORLD,
    relocate = sym minimal_rt::reloc::relocate,
    stack = sym STACK,
    STACK_SIZE = const STACK_SIZE,
    TMK_ADDRESS_LOG = const tmk_protocol::TMK_ADDRESS_LOG,
    TMK_ADDRESS_COMPLETE = const tmk_protocol::TMK_ADDRESS_COMPLETE,
}

const STACK_SIZE: usize = 4096;
#[repr(C, align(16))]
struct Stack([u8; STACK_SIZE]);
#[unsafe(no_mangle)]
static mut STACK: Stack = Stack([0; STACK_SIZE]);
