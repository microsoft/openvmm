// Copyright (C) Microsoft Corporation. All rights reserved.

//! Setting up VTL2 VPs

use crate::host_params::PartitionInfo;

pub fn setup_vtl2_vp(_partition_info: &PartitionInfo) {
    // X64 doesn't require any special VTL2 VP setup in the boot loader at the
    // moment.
}
