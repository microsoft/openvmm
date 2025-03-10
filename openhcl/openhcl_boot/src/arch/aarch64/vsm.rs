// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arch-specific VSM details.

use loader_defs::shim::SupportedIsolationType;
use minimal_rt::isolation::IsolationType;

pub fn get_isolation_type(supported_isolation_type: SupportedIsolationType) -> IsolationType {
    if supported_isolation_type != SupportedIsolationType::VBS {
        let _ = IsolationType::Vbs;
        panic!("unexpected isolation type {:?}", supported_isolation_type)
    }

    IsolationType::None
}
