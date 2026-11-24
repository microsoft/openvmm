// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use hvdef::Vtl;
use hvdef::hypercall::InitialVpContextArm64;
use zerocopy::IntoBytes;

use crate::platform::hyperv::arch::hypercall::HvCall;

impl HvCall {
    /// Starts a virtual processor (VP) with the specified VTL and context on aarch64.
    pub fn start_virtual_processor(
        &mut self,
        vp_index: u32,
        target_vtl: Vtl,
        vp_context: Option<InitialVpContextArm64>,
    ) -> Result<(), hvdef::HvError> {
        let header = hvdef::hypercall::StartVirtualProcessorArm64 {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index,
            target_vtl: target_vtl.into(),
            vp_context: vp_context.unwrap_or(zerocopy::FromZeros::new_zeroed()),
            rsvd0: 0u8,
            rsvd1: 0u16,
        };

        header
            .write_to_prefix(self.input_page().buffer.as_mut_slice())
            .expect("size of start_virtual_processor header is not correct");

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallStartVirtualProcessor, None);
        output.result()
    }

    /// Enables a VTL for a specific virtual processor (VP) on aarch64.
    pub fn enable_vp_vtl(
        &mut self,
        vp_index: u32,
        target_vtl: Vtl,
        vp_context: Option<InitialVpContextArm64>,
    ) -> Result<(), hvdef::HvError> {
        let header = hvdef::hypercall::EnableVpVtlArm64 {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index,
            // The VTL value here is just a u8 and not the otherwise usual
            // HvInputVtl value.
            target_vtl: target_vtl.into(),
            reserved: [0; 3],
            vp_vtl_context: vp_context.unwrap_or(zerocopy::FromZeros::new_zeroed()),
        };

        _ = header.write_to_prefix(self.input_page().buffer.as_mut_slice());

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallEnableVpVtl, None);
        output.result()
    }

    /// Placeholder for VTL call on aarch64.
    pub fn vtl_call() {
        unimplemented!();
    }

    /// Placeholder for VTL return on aarch64.
    pub fn vtl_return() {
        unimplemented!();
    }
}
