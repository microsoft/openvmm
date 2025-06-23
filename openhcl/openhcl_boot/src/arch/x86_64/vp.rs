// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Setting up VTL2 VPs

use crate::IsolationType;
use crate::host_params::PartitionInfo;

pub fn setup_vtl2_vp(partition_info: &PartitionInfo) {
    // Only TDX requires VP initialization in the shim on x86
    if partition_info.isolation == IsolationType::Tdx {
        crate::arch::tdx::setup_vtl2_vp(partition_info);
    };
}
