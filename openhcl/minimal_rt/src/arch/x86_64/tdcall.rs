// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use tdcall::tdcall_rdmsr;
use tdcall::Tdcall;
use tdcall::TdcallInput;
use tdcall::TdcallOutput;

/// Perform a tdcall instruction with the specified inputs.
fn tdcall(input: TdcallInput) -> TdcallOutput {
    const TD_VMCALL: u64 = 0;

    let rax: u64;
    let rcx;
    let rdx;
    let r8;
    let r10;
    let r11;

    // Since this TDCALL is used only for TDVMCALL based hypercalls,
    // check and make sure that the TDCALL is VMCALL
    assert_eq!(input.leaf.0, TD_VMCALL);

    // SAFETY: Any input registers can be output registers for VMCALL, so make sure
    // they're all inout even if the output isn't used.
    //
    unsafe {
        core::arch::asm! {
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

/// This struct implements tdcall trait and is passed in tacall functions
pub struct TdcallInstruction;

impl Tdcall for TdcallInstruction {
    fn tdcall(&mut self, input: TdcallInput) -> TdcallOutput {
        tdcall(input)
    }
}

/// Reads MSR using TDCALL
fn read_msr_tdcall(msr_index: u32) -> u64 {
    let mut msr_value: u64 = 0;
    tdcall_rdmsr(&mut TdcallInstruction, msr_index, &mut msr_value).unwrap();
    msr_value
}

/// Global variable to store tsc frequency.
static mut TSC_FREQUENCY: u64 = 0;

/// Gets the timer ref time in 100ns, and None if it fails to get it
pub fn get_tdx_tsc_reftime() -> Option<u64> {
    unsafe {
        if TSC_FREQUENCY == 0 {
            TSC_FREQUENCY = read_msr_tdcall(hvdef::HV_X64_MSR_TSC_FREQUENCY);
        }

        if TSC_FREQUENCY != 0 {
            let tsc = safe_intrinsics::rdtsc();
            let count_100ns = (tsc as u128 * 10000000) / TSC_FREQUENCY as u128;
            return Some(count_100ns as u64);
        }
    }
    None
}
