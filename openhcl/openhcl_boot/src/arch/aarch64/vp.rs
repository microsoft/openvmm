// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Setting up VTL2 VPs

use crate::hypercall::hvcall;
use crate::PartitionInfo;
crate::host_params::shim_params::IsolationType;

pub fn setup_vtl2_vp(_isolation_type: IsolationType, partition_info: &PartitionInfo) {
    // VTL2 kernel boot processor will try to remote read the GICR before AP's are
    // brought up. But, the hypervisor doesn't set the GICR overlay pages until the
    // Enable VP VTL hypercall has been made. Without the VP VTLs setup, accessing
    // GICR will cause the VTL2 kernel to panic.
    // BSP already has the VP VTL enabled at this time, so skip the BSP.
    for cpu in 1..partition_info.cpus.len() {
        hvcall()
            .enable_vp_vtl(cpu as u32)
            .expect("Enabling VP VTL should not fail");
    }
}
