// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: needed to write low-level TMK code.
#![expect(unsafe_code)]

use core::arch::global_asm;
use core::marker::PhantomData;

#[repr(C)]
struct Str<'a>(*const u8, usize, PhantomData<&'a str>);

// SAFETY: `Str` is an ABI-safe type for &str, which is Send+Sync.
unsafe impl Send for Str<'_> {}
// SAFETY: `Str` is an ABI-safe type for &str, which is Send+Sync.
unsafe impl Sync for Str<'_> {}

impl<'a> Str<'a> {
    const fn new(s: &'a str) -> Self {
        Self(s.as_ptr(), s.len(), PhantomData)
    }
}

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
    STACK_SIZE = const STACK_SIZE - 16,
    TMK_ADDRESS_LOG = const tmk_protocol::TMK_ADDRESS_LOG,
    TMK_ADDRESS_COMPLETE = const tmk_protocol::TMK_ADDRESS_COMPLETE,
}

const STACK_SIZE: usize = 4096;
#[repr(C, align(16))]
struct Stack([u8; STACK_SIZE]);
#[unsafe(no_mangle)]
static mut STACK: Stack = Stack([0; STACK_SIZE]);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    minimal_rt::arch::fault();
}
