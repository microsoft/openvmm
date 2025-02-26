// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds and publishes an OpenHCL binary for size comparison with PRs.

use crate::artifact_openvmm_hcl_sizecheck;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openvmm_hcl;
use crate::build_openvmm_hcl::OpenvmmHclBuildParams;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<artifact_openvmm_hcl_sizecheck::publish::Node>();
        ctx.import::<build_openvmm_hcl::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { done, artifact_dir } = request;

        let baseline_hcl_build = ctx.reqv(|v| build_openvmm_hcl::Request {
            build_params: OpenvmmHclBuildParams {
                target: CommonTriple::X86_64_LINUX_MUSL,
                profile: OpenvmmHclBuildProfile::OpenvmmHclShip,
                features: (OpenhclIgvmRecipe::X64)
                    .recipe_details(OpenvmmHclBuildProfile::OpenvmmHclShip)
                    .openvmm_hcl_features,
                no_split_dbg_info: false,
            },
            openvmm_hcl_output: v,
        });

        ctx.req(artifact_openvmm_hcl_sizecheck::publish::Request {
            openvmm_openhcl_x86: baseline_hcl_build,
            artifact_dir,
            done,
        });

        Ok(())
    }
}
