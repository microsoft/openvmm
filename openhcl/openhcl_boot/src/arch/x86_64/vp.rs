// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Setting up VTL2 VPs

use crate::host_params::PartitionInfo;
use crate::hvcall;
use crate::host_params::shim_params::IsolationType;

/// Perform any initialization required for APs in the bootshim. On TDX, /// this puts
/// the APs into the correct state and starts them by invoking TDVMCALL-based hypercall.
/// Otherwise, this function is a noop
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
