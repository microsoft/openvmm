// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Setting up VTL2 VPs

use crate::IsolationType;
use crate::host_params::PartitionInfo;

pub fn setup_vtl2_vp(partition_info: &PartitionInfo) {
    // Non-Isolated X64 doesn't require any special VTL2 VP setup in the boot loader
    // at the moment.
    match partition_info.isolation {
        IsolationType::Tdx => crate::arch::tdx::setup_vtl2_vp(partition_info),
        _ => (),
    };
}
