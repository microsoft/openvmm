// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Setting up VTL2 VPs

use crate::host_params::PartitionInfo;
use crate::host_params::shim_params::IsolationType;
use crate::hvcall;

/// VTL2 VP setup for x86_64
///
/// If the partition is TDX isolated partitions, invoke TDVMCALLs
/// to enabled VTL2 and start the VPs. Otherwise do nothing.
pub fn setup_vtl2_vp(isolation_type: IsolationType, partition_info: &PartitionInfo) {
    if isolation_type == IsolationType::Tdx {
        for cpu in 1..partition_info.cpus.len() {
            hvcall()
                .enable_vp_vtl(cpu as u32)
                .expect("enabling vp should not fail");
        }

        // Start VPs on Tdx-isolated VMs by sending TDVMCALL-based hypercall HvCallStartVirtualProcessor
        for cpu in 1..partition_info.cpus.len() {
            hvcall()
                .start_vp(cpu as u32)
                .expect("start vp should not fail");
        }
    }
}
