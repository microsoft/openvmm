// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds a `vmfirmwareigvm`-style resource DLL wrapping a specific OpenHCL
//! IGVM file and publishes it as a typed artifact, for use in VMM tests that
//! exercise the `vmgstool copy-igvmfile` flow.
//!
//! The IGVM payload is taken from a previously-published, typed OpenHCL IGVM
//! artifact (see [`crate::build_openhcl_igvm_from_recipe::OpenhclIgvmOutput`]).
//! The caller is responsible for selecting the desired recipe's IGVM and
//! passing it in.

use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmOutput;
use crate::build_vmfirmwareigvm_test_dll::VmfirmwareigvmTestDllOutput;
use crate::common::CommonArch;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        /// Architecture of the resulting DLL.
        pub arch: CommonArch,
        /// The OpenHCL IGVM whose payload should be embedded in the DLL.
        pub openhcl_igvm: ReadVar<OpenhclIgvmOutput>,
        /// Where to publish the resulting DLL.
        pub vmfirmwareigvm_test_dll_artifact: WriteVar<VmfirmwareigvmTestDllOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_vmfirmwareigvm_test_dll::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            arch,
            openhcl_igvm,
            vmfirmwareigvm_test_dll_artifact,
        } = request;

        let igvm_bin = openhcl_igvm.map(ctx, |igvm| igvm.igvm_bin().to_path_buf());

        ctx.req(crate::build_vmfirmwareigvm_test_dll::Request {
            arch,
            igvm_bin,
            vmfirmwareigvm_test_dll: vmfirmwareigvm_test_dll_artifact,
        });

        Ok(())
    }
}
