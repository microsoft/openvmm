// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::msr::read_msr;
use super::tdcall::get_tdx_tsc_reftime;
use crate::isolation::IsolationType;

pub fn reference_time(isolation: IsolationType) -> Option<u64> {
    match isolation {
        IsolationType::Tdx => get_tdx_tsc_reftime(),
        IsolationType::Snp => None,
        _ => {
            // SAFETY: no safety requirements.
            unsafe { Some(read_msr(hvdef::HV_X64_MSR_TIME_REF_COUNT)) }
        }
    }
}
