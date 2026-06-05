// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds a `vmfirmwareigvm`-style resource DLL wrapping a specific OpenHCL
//! IGVM file and publishes it as a typed artifact, for use in VMM tests that
//! exercise the `vmgstool copy-igvmfile` flow.
//!
//! The IGVM payload is read from a previously-published openhcl IGVM
//! artifact directory (see [`crate::artifact_openhcl_igvm_from_recipe`]).
//! The expected recipe (e.g. `OpenhclIgvmRecipe::X64`) determines which
//! file in the directory is wrapped.

use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_vmfirmwareigvm_test_dll::VmfirmwareigvmTestDllOutput;
use crate::common::CommonArch;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        /// Architecture of the resulting DLL.
        pub arch: CommonArch,
        /// Which recipe's IGVM should be embedded.
        pub recipe: OpenhclIgvmRecipe,
        /// Directory containing the openhcl IGVM artifact (as published by
        /// [`crate::artifact_openhcl_igvm_from_recipe::publish`]).
        pub openhcl_igvm_artifact_dir: ReadVar<PathBuf>,
        /// Where to publish the resulting DLL.
        pub vmfirmwareigvm_test_dll_artifact: WriteVar<VmfirmwareigvmTestDllOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_openhcl_igvm_from_recipe::resolve::Node>();
        ctx.import::<crate::build_vmfirmwareigvm_test_dll::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            arch,
            recipe,
            openhcl_igvm_artifact_dir,
            vmfirmwareigvm_test_dll_artifact,
        } = request;

        // Resolve the openhcl IGVM artifact directory into a list of
        // (recipe, IgvmOutput) pairs.
        let all_igvm_files =
            ctx.reqv(
                |igvm_files| crate::artifact_openhcl_igvm_from_recipe::resolve::Request {
                    artifact_dir: openhcl_igvm_artifact_dir,
                    igvm_files,
                },
            );

        // Pick out the IGVM matching the requested recipe.
        let recipe_discriminant = std::mem::discriminant(&recipe);
        let igvm = all_igvm_files.map(ctx, move |files| {
            files
                .into_iter()
                .find(|(r, _)| std::mem::discriminant(r) == recipe_discriminant)
                .map(|(_, igvm)| igvm)
                .unwrap_or_else(|| {
                    panic!(
                        "openhcl IGVM artifact directory does not contain a build \
                         for the requested recipe {recipe:?}"
                    )
                })
        });

        ctx.req(crate::build_vmfirmwareigvm_test_dll::Request {
            arch,
            igvm,
            vmfirmwareigvm_test_dll: vmfirmwareigvm_test_dll_artifact,
        });

        Ok(())
    }
}
