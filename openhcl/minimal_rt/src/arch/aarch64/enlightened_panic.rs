// Copyright (C) Microsoft Corporation. All rights reserved.

//! Enlightened panic registers for an ARM64 Hyper-V guest.

use super::hypercall::set_register_fast;
use hvdef::HvArm64RegisterName;

const REGS: [HvArm64RegisterName; 6] = [
    HvArm64RegisterName::GuestCrashP0,
    HvArm64RegisterName::GuestCrashP1,
    HvArm64RegisterName::GuestCrashP2,
    HvArm64RegisterName::GuestCrashP3,
    HvArm64RegisterName::GuestCrashP4,
    HvArm64RegisterName::GuestCrashCtl,
];

// SAFETY: Caller must ensure that the Hyper-V TLFS contract is followed.
pub unsafe fn write_crash_reg(index: usize, value: u64) {
    let _ = set_register_fast(REGS[index].into(), value.into());
}
