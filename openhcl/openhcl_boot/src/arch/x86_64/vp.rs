// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Setting up VTL2 VPs

use crate::host_params::PartitionInfo;
use crate::hypercall::HvCall;
pub fn setup_vtl2_vp(_: &mut HvCall, _partition_info: &PartitionInfo) {
    // X64 doesn't require any special VTL2 VP setup in the boot loader at the
    // moment.
}
