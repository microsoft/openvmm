// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arch-specific VSM details.

use crate::isolation::IsolationType;
use loader_defs::shim::SupportedIsolationType;

pub fn get_isolation_type(supported_isolation_type: SupportedIsolationType) -> IsolationType {
    match supported_isolation_type {
        SupportedIsolationType::VBS => {
            let cpuid_result = safe_intrinsics::cpuid(hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES, 0);
            let privs = cpuid_result.eax as u64 | ((cpuid_result.ebx as u64) << 32);
            if hvdef::HvPartitionPrivilege::from(privs).isolation() {
                IsolationType::Vbs
            } else {
                IsolationType::None
            }
        }
        SupportedIsolationType::SNP => IsolationType::Snp,
        SupportedIsolationType::TDX => IsolationType::Tdx,
        _ => panic!("unexpected isolation type"),
    }
}
