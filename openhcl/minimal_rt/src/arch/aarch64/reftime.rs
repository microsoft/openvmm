// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::hypercall::get_register_fast;
use crate::isolation::IsolationType;

pub fn reference_time(_isolation: IsolationType) -> Option<u64> {
    Some(
        get_register_fast(hvdef::HvArm64RegisterName::TimeRefCount.into())
            .expect("failed to query reference time")
            .as_u64(),
    )
}
