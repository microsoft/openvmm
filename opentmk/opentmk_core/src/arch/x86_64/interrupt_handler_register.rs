// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64 interrupt entry stubs and IDT registration.
//!
//! Uses assembly interrupt entry stubs (one per vector) that funnel into a
//! single `extern "sysv64"` handler, so the nightly `abi_x86_interrupt` feature
//! is not required. Stub addresses are installed into the IDT via the stable
//! [`x86_64::structures::idt::Entry::set_handler_addr`].
//!
//! `isr_common` preserves the full architectural register state across the
//! handler: the integer GPRs plus x87/SSE state (x87, MXCSR, XMM0-15) via
//! `fxsave64`/`fxrstor64`. The `x86_64-unknown-uefi` target is built for the
//! SSE2 baseline (no AVX), so `fxsave64` captures the complete vector state; if
//! AVX is ever enabled here, switch to `xsave`.

use x86_64::VirtAddr;
use x86_64::structures::idt::InterruptDescriptorTable;

/// Length of each entry in [`ISR0`]: a `push imm32` of the vector number (5
/// bytes) followed by a `jmp rel32` to `isr_common` (5 bytes).
const ISR_STUB_LEN: usize = 10;

#[cfg(target_feature = "avx")]
compile_error!("AVX is not supported by this module");

core::arch::global_asm! {
    ".globl isr_common",
    "isr_common:",
    "push rbp",
    "push r11",
    "push r10",
    "push r9",
    "push r8",
    "push rdi",
    "push rsi",
    "push rdx",
    "push rcx",
    "push rax",
    "mov rbp, rsp",
    "mov rdi, rbp",                 // arg0 = pointer to saved Frame
    "sub rsp, 512",                 // reserve a 512-byte FXSAVE area below the frame
    "and rsp, 0xfffffffffffffff0",  // 16-byte align it (fxsave requires it; also aligns the call)
    "fxsave64 [rsp]",               // preserve the interrupted x87/MXCSR/XMM state
    "call {isr_handler}",
    "fxrstor64 [rsp]",              // restore x87/MXCSR/XMM (leaves al and flags intact)
    "test al, al",                  // al = whether the vector pushed an error code
    "mov rsp, rbp",                 // discard the FXSAVE area, back to the saved frame
    "pop rax",
    "pop rcx",
    "pop rdx",
    "pop rsi",
    "pop rdi",
    "pop r8",
    "pop r9",
    "pop r10",
    "pop r11",
    "pop rbp",
    "jz 2f",
    "add rsp, 8",                   // has error code: pop the vector too
    "2:",
    "add rsp, 8",                   // pop the error code or the vector
    "iretq",

    // 256 interrupt entry points, each `push <vector>; jmp isr_common`.
    ".globl {isr0}",
    "{isr0}:",
    ".rept 256",
    ".byte 0x68", ".long \\+",                  // push imm32 (the vector number)
    ".byte 0xe9", ".long isr_common - . - 4",   // jmp rel32 to isr_common
    ".endr",
    isr_handler = sym isr_handler,
    isr0 = sym ISR0,
}

unsafe extern "C" {
    /// Base of the 256 assembly interrupt entry stubs defined in `global_asm!`.
    //  SAFETY: ISR0 is used as reference symbol, we don't actually read/write to it,
    //  the handler operations are already marked as unsafe when handlers defined
    //  in global_asm are called.
    safe static ISR0: [u8; 256 * ISR_STUB_LEN];
}

/// Volatile register state saved by `isr_common` before calling [`isr_handler`].
#[repr(C)]
struct Frame {
    rax: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    rbp: u64,
    vector: u64,
}

/// Whether the CPU pushes an error code for `vector`.
fn has_error_code(vector: u8) -> bool {
    matches!(vector, 8 | 10..=14 | 17 | 21 | 29 | 30)
}

/// Common interrupt handler, called by `isr_common`.
///
/// Returns whether the vector pushed an error code so the assembly epilogue can
/// pop it before `iretq`.
///
/// Uses the SysV ABI (argument in `rdi`, no shadow space) to match the
/// hand-written `isr_common` trampoline; the target's `extern "C"` would be the
/// Win64 ABI on `x86_64-unknown-uefi`.
///
/// # Safety
/// Must only be called from `isr_common` with a valid pointer to the saved
/// [`Frame`] on the interrupt stack. Invalid Frame pointers can cause
/// undefined behavior.
unsafe extern "sysv64" fn isr_handler(frame: *mut Frame) -> bool {
    // SAFETY: Caller guarantees a valid pointer to the saved frame.
    let vector = unsafe { (*frame).vector as u8 };
    super::interrupt::dispatch(vector);
    has_error_code(vector)
}

/// Address of the entry stub for `vector`.
fn isr_addr(vector: usize) -> VirtAddr {
    VirtAddr::new(core::ptr::addr_of!(ISR0) as u64 + (vector * ISR_STUB_LEN) as u64)
}

/// Points every IDT entry at its assembly entry stub.
pub fn register_interrupt_handler(idt: &mut InterruptDescriptorTable) {
    // SAFETY: each address is a valid interrupt entry point (defined in the
    // `global_asm!` block above) whose stub handles any entry type correctly.
    unsafe {
        idt.divide_error.set_handler_addr(isr_addr(0));
        idt.debug.set_handler_addr(isr_addr(1));
        idt.non_maskable_interrupt.set_handler_addr(isr_addr(2));
        idt.breakpoint.set_handler_addr(isr_addr(3));
        idt.overflow.set_handler_addr(isr_addr(4));
        idt.bound_range_exceeded.set_handler_addr(isr_addr(5));
        idt.invalid_opcode.set_handler_addr(isr_addr(6));
        idt.device_not_available.set_handler_addr(isr_addr(7));
        idt.double_fault.set_handler_addr(isr_addr(8));
        idt[9u8].set_handler_addr(isr_addr(9));
        idt.invalid_tss.set_handler_addr(isr_addr(10));
        idt.segment_not_present.set_handler_addr(isr_addr(11));
        idt.stack_segment_fault.set_handler_addr(isr_addr(12));
        idt.general_protection_fault.set_handler_addr(isr_addr(13));
        idt.page_fault.set_handler_addr(isr_addr(14));
        idt.x87_floating_point.set_handler_addr(isr_addr(16));
        idt.alignment_check.set_handler_addr(isr_addr(17));
        idt.machine_check.set_handler_addr(isr_addr(18));
        idt.simd_floating_point.set_handler_addr(isr_addr(19));
        idt.virtualization.set_handler_addr(isr_addr(20));
        idt.cp_protection_exception.set_handler_addr(isr_addr(21));
        idt.hv_injection_exception.set_handler_addr(isr_addr(28));
        idt.vmm_communication_exception
            .set_handler_addr(isr_addr(29));
        idt.security_exception.set_handler_addr(isr_addr(30));
        // Vectors 15, 22-27, and 31 are Intel-reserved: the x86_64 crate's IDT
        // API cannot set them and hardware never delivers them. A spurious
        // delivery would fault through the #GP (vector 13) stub rather than
        // silently, so leave them unset.
        for vector in 32..=255usize {
            idt[vector as u8].set_handler_addr(isr_addr(vector));
        }
    }
}
