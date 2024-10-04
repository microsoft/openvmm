// Copyright (C) Microsoft Corporation. All rights reserved.

//! TDX support.

use core::arch::asm;
use memory_range::MemoryRange;
use tdcall::tdcall_map_gpa;
use tdcall::AcceptPagesError;
use tdcall::Tdcall;
use tdcall::TdcallInput;
use tdcall::TdcallOutput;

/// Perform a tdcall instruction with the specified inputs.
fn tdcall(input: TdcallInput) -> TdcallOutput {
    let rax: u64;
    let rcx;
    let rdx;
    let r8;
    let r10;
    let r11;

    // Any input registers can be output registers for VMCALL, so make sure
    // they're all inout even if the output isn't used.
    //
    // FUTURE: consider not allowing VMCALL through this path, to avoid needing
    // to save/restore as many registers. Hard code that separately.
    //
    // SAFETY: Calling tdcall with the correct arguments. It is responsible for
    // argument validation and error handling.
    unsafe {
        asm! {
            "tdcall",
            inout("rax") input.leaf.0 => rax,
            inout("rcx") input.rcx => rcx,
            inout("rdx") input.rdx => rdx,
            inout("r8") input.r8 => r8,
            inout("r9")  input.r9 => _,
            inout("r10") input.r10 => r10,
            inout("r11") input.r11 => r11,
            inout("r12") input.r12 => _,
            inout("r13") input.r13 => _,
            inout("r14") input.r14 => _,
            inout("r15") input.r15 => _,
        }
    }

    TdcallOutput {
        rax: rax.into(),
        rcx,
        rdx,
        r8,
        r10,
        r11,
    }
}

pub struct TdcallInstruction;

impl Tdcall for TdcallInstruction {
    fn tdcall(&mut self, input: TdcallInput) -> TdcallOutput {
        tdcall(input)
    }
}

/// Accept pages from the specified range.
pub fn accept_pages(range: MemoryRange) -> Result<(), AcceptPagesError> {
    tdcall::accept_pages(
        &mut TdcallInstruction,
        range,
        tdcall::AcceptPagesAttributes::None,
    )
}

/// Change the visibility of pages. Note that pages that were previously host
/// visible and are now private, must be reaccepted.
pub fn change_page_visibility(range: MemoryRange, host_visible: bool) {
    if let Err(err) = tdcall_map_gpa(&mut TdcallInstruction, range, host_visible) {
        panic!(
            "failed to change page visibility for {range}, host_visible = {host_visible}: {err:?}"
        );
    }
}

/// Tdcall based io port access.
pub struct TdxIoAccess;

impl minimal_rt::arch::IoAccess for TdxIoAccess {
    unsafe fn inb(&self, port: u16) -> u8 {
        tdcall::tdcall_io_in(&mut TdcallInstruction, port, 1).unwrap() as u8
    }

    unsafe fn outb(&self, port: u16, data: u8) {
        let _ = tdcall::tdcall_io_out(&mut TdcallInstruction, port, data as u32, 1);
    }
}
