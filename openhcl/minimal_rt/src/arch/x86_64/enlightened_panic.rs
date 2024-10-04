// Copyright (C) Microsoft Corporation. All rights reserved.

//! Enlightened panic registers for an x64 Hyper-V guest.

use crate::arch::x86_64::msr::write_msr;
use hvdef::HV_X64_MSR_GUEST_CRASH_CTL;
use hvdef::HV_X64_MSR_GUEST_CRASH_P0;
use hvdef::HV_X64_MSR_GUEST_CRASH_P1;
use hvdef::HV_X64_MSR_GUEST_CRASH_P2;
use hvdef::HV_X64_MSR_GUEST_CRASH_P3;
use hvdef::HV_X64_MSR_GUEST_CRASH_P4;

const REGS: [u32; 6] = [
    HV_X64_MSR_GUEST_CRASH_P0,
    HV_X64_MSR_GUEST_CRASH_P1,
    HV_X64_MSR_GUEST_CRASH_P2,
    HV_X64_MSR_GUEST_CRASH_P3,
    HV_X64_MSR_GUEST_CRASH_P4,
    HV_X64_MSR_GUEST_CRASH_CTL,
];

/// # Safety
///
/// Caller must ensure that the Hyper-V TLFS contract is followed.
pub unsafe fn write_crash_reg(index: usize, value: u64) {
    // SAFETY: Caller guaranteed.
    unsafe {
        write_msr(REGS[index], value);
    }
}
