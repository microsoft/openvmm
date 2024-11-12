// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::msr::read_msr;

pub fn reference_time() -> u64 {
    // SAFETY: no safety requirements.
    unsafe { read_msr(hvdef::HV_X64_MSR_TIME_REF_COUNT) }
}
