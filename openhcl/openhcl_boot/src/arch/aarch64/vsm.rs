// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arch-specific VSM details.

use crate::host_params::shim_params::IsolationType;
use loader_defs::shim::SupportedIsolationType;

pub fn get_isolation_type(supported_isolation_type: SupportedIsolationType) -> IsolationType {
    if supported_isolation_type != SupportedIsolationType::VBS {
        let _ = IsolationType::Vbs;
        panic!("unexpected isolation type {:?}", supported_isolation_type)
    }

    IsolationType::None
}
