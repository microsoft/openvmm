// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64 entry point and support.

#![cfg(target_arch = "x86_64")]

use super::Scope;
use core::marker::PhantomData;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering::Relaxed;
use x86defs::IdtAttributes;
use x86defs::RFlags;

#[cfg(minimal_rt)]
mod entry {
    core::arch::global_asm! {
        ".globl _start",
        "_start:",
        "mov r12, rsi",
        "lea rsp, {STACK_SIZE} + {stack}[rip]",
        "lea rdx, _DYNAMIC[rip]",
        "lea rdi, __ehdr_start[rip]",
        "mov rsi, rdi",
        "call {relocate}",
        "call {arch_init}",
        "mov rdi, r12",
        "jmp {entry}",
        relocate = sym minimal_rt::reloc::relocate,
        stack = sym STACK,
        STACK_SIZE = const STACK_SIZE,
        entry = sym crate::entry,
        arch_init = sym super::arch_init,
    }

    const STACK_SIZE: usize = 65536;
    #[repr(C, align(16))]
    struct Stack([u8; STACK_SIZE]);
    static mut STACK: Stack = Stack([0; STACK_SIZE]);

    core::arch::global_asm! {
        ".globl isr_common",
        "isr_common:",
        "push rbp",
        "mov rbp, rsp",
        "push rax",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "mov rdi, rbp",
        "and rsp, 0xfffffffffffffff0", // align to 16 bytes
        "call {isr_handler}",
        "test al, al", // check if there's an error code on the stack
        "mov rax, [rbp - 8]",
        "mov rcx, [rbp - 16]",
        "mov rdx, [rbp - 24]",
        "mov rsi, [rbp - 32]",
        "mov rdi, [rbp - 40]",
        "mov r8, [rbp - 48]",
        "mov r9, [rbp - 56]",
        "mov r10, [rbp - 64]",
        "mov r11, [rbp - 72]",
        "lea rsp, [rbp + 16]", // pop the stack frame (including rbp and vector)
        "jz 2f",
        "add rsp, 8", // pop the error code
        "2:",
        "iretq",

        // Define the 256 interrupt entry points. Repeat 256 times.
        ".globl {isr0}",
        "{isr0}:",
        ".rept 256",
        ".byte 0x68", ".long \\+",                  // push vector
        ".byte 0xe9", ".long isr_common - . - 4",   // jmp isr_common
        ".endr",
        isr_handler = sym super::isr_handler,
        isr0 = sym super::ISR0,
    }
}

/// A context passed to the ISR handler.
pub struct IsrContext<'a> {
    /// The error code for the exception, if applicable.
    pub error_code: Option<u64>,
    /// The instruction pointer at the time of the exception or interrupt.
    ///
    /// This can be modified by the ISR handler to change the instruction
    /// pointer after the exception.
    pub rip: u64,
    _phantom: PhantomData<&'a mut ()>,
}

/// # Safety
/// Must be called from a valid ISR context.
#[cfg_attr(not(minimal_rt), expect(dead_code))]
unsafe extern "C" fn isr_handler(frame: *mut u64) -> bool {
    #[repr(C)]
    struct Frame {
        rbp: u64,
        vector: u64,
        error_code: u64,
    }
    // SAFETY: caller ensures this is a valid pointer to a stack frame.
    let frame_up = unsafe { &*frame.cast::<Frame>() };
    let vector = frame_up.vector as u8;

    let has_error_code = matches!(vector, 1 | 8 | 0xa..=0xe | 0x11 | 0x12 | 0x15 | 0x1d | 0x1e);

    #[repr(C)]
    struct ReturnParams {
        rip: u64,
        cs: u64,
        rflags: u64,
    }

    // SAFETY: caller ensures the frame pointer has the return parameters here.
    let return_params = unsafe {
        &mut *frame
            .byte_add(if has_error_code { 24 } else { 16 })
            .cast::<ReturnParams>()
    };

    let error_code = has_error_code.then_some(frame_up.error_code);

    // SAFETY: `ISRS` is not modified with interrupts disabled.
    let isr = unsafe { ISRS[vector as usize] };
    // SAFETY: this is the underlying type of the ISR.
    let isr = unsafe {
        core::mem::transmute::<[usize; 2], Option<&(dyn Send + Fn(&mut IsrContext<'_>))>>(isr)
    };
    let Some(isr) = isr else {
        panic!(
            "unhandled interrupt: vector = {vector:?}, error_code = {error_code:#x}, rip = {rip:#x}",
            vector = x86defs::Exception(vector),
            error_code = error_code.unwrap_or(0),
            rip = return_params.rip,
        );
    };

    let mut ctx = IsrContext {
        error_code,
        rip: return_params.rip,
        _phantom: PhantomData,
    };
    isr(&mut ctx);
    return_params.rip = ctx.rip;
    has_error_code
}

unsafe extern "C" {
    safe static ISR0: [u8; 2560];
}

static mut ISRS: [[usize; 2]; 256] = [[0; 2]; 256];

pub(super) struct ArchScopeState {
    old_isrs: Option<[[usize; 2]; 256]>,
    interrupt_state: bool,
}

#[cfg_attr(not(minimal_rt), expect(dead_code))]
extern "C" fn arch_init() {
    static GDT: [x86defs::GdtEntry; 4] = {
        let default_data_attributes = x86defs::X64_DEFAULT_DATA_SEGMENT_ATTRIBUTES.as_bits();
        let default_code_attributes = x86defs::X64_DEFAULT_CODE_SEGMENT_ATTRIBUTES.as_bits();
        let zero = x86defs::GdtEntry {
            limit_low: 0,
            base_low: 0,
            base_middle: 0,
            attr_low: 0,
            attr_high: 0,
            base_high: 0,
        };

        [
            zero,
            zero,
            x86defs::GdtEntry {
                limit_low: 0xffff,
                attr_low: default_code_attributes as u8,
                attr_high: (default_code_attributes >> 8) as u8,
                ..zero
            },
            x86defs::GdtEntry {
                limit_low: 0xffff,
                attr_low: default_data_attributes as u8,
                attr_high: (default_data_attributes >> 8) as u8,
                ..zero
            },
        ]
    };

    static mut IDT: [x86defs::IdtEntry64; 256] = {
        let zero = x86defs::IdtEntry64 {
            offset_low: 0,
            selector: 0,
            attributes: IdtAttributes::new(),
            offset_middle: 0,
            offset_high: 0,
            reserved: 0,
        };
        [zero; 256]
    };

    let idt = core::array::from_fn(|i| {
        let isr = &raw const ISR0 as usize + i * 10;
        x86defs::IdtEntry64 {
            offset_low: isr as u16,
            selector: 2 * 8,
            attributes: IdtAttributes::new().with_present(true).with_gate_type(0xf),
            offset_middle: (isr >> 16) as u16,
            offset_high: (isr >> 32) as u32,
            reserved: 0,
        }
    });
    // SAFETY: IDT is not yet aliased.
    unsafe { IDT = idt };

    #[repr(C, packed)]
    struct LidtInput {
        limit: u16,
        base: u64,
    }
    let gdt = LidtInput {
        limit: size_of_val(&GDT) as u16 - 1,
        base: &raw const GDT as u64,
    };
    // SAFETY: GDT is initialized.
    unsafe {
        core::arch::asm! {
            "lgdt [{gdt}]",
            // Return to the next instruction to reload CS.
            "push {cs}",
            "lea rax, 2f[rip]",
            "push rax",
            "retfq",
            "2:",
            "mov ss, {ss}",
            cs = const 0x10,
            ss = in(reg) 0x18u64,
            gdt = in(reg) &gdt,
            out("rax") _,
        }
    }

    let idt = LidtInput {
        // SAFETY: just getting the IDT size.
        limit: size_of_val(unsafe { &*{ &raw const IDT } }) as u16 - 1,
        base: &raw const IDT as u64,
    };
    // SAFETY: IDT is initialized.
    unsafe {
        core::arch::asm! {
            "lidt [{idt}]",
            idt = in(reg) &idt,
        }
    }
}

impl<'scope> Scope<'scope, '_> {
    pub(super) fn arch_init() -> ArchScopeState {
        ArchScopeState {
            old_isrs: None,
            interrupt_state: are_interrupts_enabled(),
        }
    }

    pub(super) fn arch_reset(&mut self) {
        if let Some(isrs) = self.arch.old_isrs.take() {
            let _disable = disable_guarded();
            // SAFETY: ISRS is not concurrently accessed while interrupts are
            // disabled.
            unsafe { ISRS = isrs };
        }
        if self.arch.interrupt_state {
            enable_interrupts();
        } else {
            disable_interrupts();
        }
    }

    fn set_idt(&mut self) {
        if self.arch.old_isrs.is_some() {
            return;
        }
        let _disable = disable_guarded();
        // SAFETY: ISRS is not concurrent modified.
        self.arch.old_isrs = Some(unsafe { ISRS });
    }

    /// Sets an interrupt service routine for the given vector.
    ///
    /// This is reverted when the scope ends.
    pub fn set_isr(&mut self, vector: u8, handler: &'scope (dyn Send + Fn(&mut IsrContext<'_>))) {
        self.set_idt();
        let _disable = disable_guarded();
        // SAFETY: ISRS is not concurrently accessed while interrupts are
        // disabled.
        unsafe {
            ISRS[vector as usize] =
                core::mem::transmute::<&dyn Fn(&mut IsrContext<'_>), [usize; 2]>(handler)
        };
    }

    /// Reads the specified MSR, returning an error if the read causes a general
    /// protection fault.
    pub fn read_msr(&mut self, v: u32) -> Result<u64, Gpf> {
        let faulted = AtomicBool::new(false);
        let handler = |ctx: &mut IsrContext<'_>| {
            faulted.store(true, Relaxed);
            ctx.rip += 2;
        };
        self.subscope(|s| {
            s.set_isr(x86defs::Exception::GENERAL_PROTECTION_FAULT.0, &handler);
            let mut low = 0u32;
            let mut high = 0u32;
            // SAFETY: reading an MSR is safe, especially since we have an
            // exception handler registered.
            unsafe {
                core::arch::asm! {
                    "rdmsr",
                    in("ecx") v,
                    inout("eax") low,
                    inout("rdx") high,
                }
            }
            if faulted.load(Relaxed) {
                return Err(Gpf);
            }
            Ok(((high as u64) << 32) | low as u64)
        })
    }

    /// Writes the specified MSR, returning an error if the write causes a
    /// general protection fault.
    pub fn write_msr(&mut self, v: u32, value: u64) -> Result<(), Gpf> {
        let faulted = AtomicBool::new(false);
        let handler = |ctx: &mut IsrContext<'_>| {
            faulted.store(true, Relaxed);
            ctx.rip += 2;
        };
        self.subscope(|s| {
            s.set_isr(x86defs::Exception::GENERAL_PROTECTION_FAULT.0, &handler);
            let low = value as u32;
            let high = (value >> 32) as u32;
            // SAFETY: caller ensures writing the MSR is safe.
            unsafe {
                core::arch::asm! {
                    "wrmsr",
                    in("ecx") v,
                    in("rax") low,
                    in("rdx") high,
                }
            }
            if faulted.load(Relaxed) {
                return Err(Gpf);
            }
            Ok(())
        })
    }

    /// Enables interrupts.
    ///
    /// Reverts when the scope ends.
    pub fn enable_interrupts(&self) {
        enable_interrupts();
    }

    /// Disables interrupts and returns true if they were previously enabled.
    ///
    /// Reverts when the scope ends.
    pub fn disable_interrupts(&self) -> bool {
        disable_interrupts()
    }
}

/// A general protection fault occurred.
#[derive(Debug)]
pub struct Gpf;

#[must_use]
struct DisableGuard(bool);

fn disable_guarded() -> DisableGuard {
    let interrupts_enabled = disable_interrupts();
    DisableGuard(interrupts_enabled)
}

impl Drop for DisableGuard {
    fn drop(&mut self) {
        if self.0 {
            enable_interrupts();
        }
    }
}

fn disable_interrupts() -> bool {
    let enabled = are_interrupts_enabled();
    if enabled {
        // SAFETY: disabling interrupts is always memory safe.
        unsafe {
            core::arch::asm!("cli");
        }
    }
    enabled
}

fn enable_interrupts() {
    // SAFETY: caller ensures this is safe.
    unsafe {
        core::arch::asm!("sti");
    }
}

fn are_interrupts_enabled() -> bool {
    let mut flags: u64;
    // SAFETY: just reading flags.
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {flags}",
            flags = out(reg) flags,
        );
    }
    RFlags::from(flags).interrupt_enable()
}
