// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDX support.

use crate::single_threaded::SingleThreaded;
use core::arch::asm;
use core::cell::Cell;
use memory_range::MemoryRange;
use safe_intrinsics::cpuid;
use tdcall::AcceptPagesError;
use tdcall::Tdcall;
use tdcall::TdcallInput;
use tdcall::TdcallOutput;
use tdcall::tdcall_map_gpa;

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

/// Global variable to store tsc frequency.
static TSC_FREQUENCY: SingleThreaded<Cell<u64>> = SingleThreaded(Cell::new(0));

/// Gets the timer ref time in 100ns, and None if it fails to get it
pub fn get_tdx_tsc_reftime() -> Option<u64> {
    // This is first called by the BSP from openhcl_boot and the frequency
    // is saved in this gloabal variable. Subsequent calls use the global variable.
    if TSC_FREQUENCY.get() == 0 {
        // The TDX module interprets frequencies as multiples of 25 MHz
        const TDX_FREQ_MULTIPLIER: u64 = 25 * 1000 * 1000;
        const CPUID_LEAF_TDX_TSC_FREQ: u32 = 0x15;
        TSC_FREQUENCY.set(cpuid(CPUID_LEAF_TDX_TSC_FREQ, 0x0).ebx as u64 * TDX_FREQ_MULTIPLIER);
    }

    if TSC_FREQUENCY.get() != 0 {
        let tsc = safe_intrinsics::rdtsc();
        let count_100ns = (tsc as u128 * 10000000) / TSC_FREQUENCY.get() as u128;
        return Some(count_100ns as u64);
    }
    None
}
