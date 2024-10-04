// Copyright (C) Microsoft Corporation. All rights reserved.

use super::hypercall::get_register_fast;

pub fn reference_time() -> u64 {
    get_register_fast(hvdef::HvArm64RegisterName::TimeRefCount.into())
        .expect("failed to query reference time")
        .as_u64()
}
