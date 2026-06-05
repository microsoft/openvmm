// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build a `vmfirmwareigvm`-style resource DLL wrapping a given OpenHCL IGVM
//! file, for use in VMM tests that exercise the `vmgstool copy-igvmfile` flow.
//!
//! This node is a thin wrapper around [`crate::build_vmfirmwareigvm_dll`] that
//! picks a fixed (non-production) DLL name/version and produces a typed
//! artifact. The caller is responsible for providing the IGVM payload (e.g.
//! by separately invoking [`crate::build_openhcl_igvm_from_recipe`]).

use crate::build_vmfirmwareigvm_dll::VmfirmwareigvmDllOutput;
use crate::common::CommonArch;
use crate::run_igvmfilegen::IgvmOutput;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum VmfirmwareigvmTestDllOutput {
    Dll {
        #[serde(rename = "vmfirmwareigvm.dll")]
        dll: PathBuf,
    },
}

impl Artifact for VmfirmwareigvmTestDllOutput {}

flowey_request! {
    pub struct Request {
        /// Target architecture for the DLL.
        pub arch: CommonArch,
        /// The IGVM file to embed as the `VMFW`/`NONCONFIDENTIAL` resource.
        pub igvm: ReadVar<IgvmOutput>,
        /// The resulting DLL output.
        pub vmfirmwareigvm_test_dll: WriteVar<VmfirmwareigvmTestDllOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_vmfirmwareigvm_dll::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            arch,
            igvm,
            vmfirmwareigvm_test_dll,
        } = request;

        let dll = ctx.reqv(|v| crate::build_vmfirmwareigvm_dll::Request {
            arch,
            igvm,
            // Fixed version so the DLL is stable across rebuilds. The exact
            // value is not significant (it is not used at runtime), but a
            // distinct value makes it obvious in metadata that this DLL was
            // produced from a non-production build.
            dll_version: ReadVar::from_static((1, 0, 1337, 0)),
            internal_dll_name: "vmfirmwareigvm.dll".into(),
            vmfirmwareigvm_dll: v,
        });

        ctx.emit_minor_rust_step("report built vmfirmwareigvm test DLL", |ctx| {
            let dll = dll.claim(ctx);
            let vmfirmwareigvm_test_dll = vmfirmwareigvm_test_dll.claim(ctx);
            move |rt| {
                let VmfirmwareigvmDllOutput { dll } = rt.read(dll);
                rt.write(
                    vmfirmwareigvm_test_dll,
                    &VmfirmwareigvmTestDllOutput::Dll { dll },
                );
            }
        });

        Ok(())
    }
}
