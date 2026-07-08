// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds and publishes an OpenHCL binary for size comparison with PRs.

use crate::artifact_openvmm_hcl_sizecheck;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openvmm_hcl;
use crate::build_openvmm_hcl::OpenvmmHclBuildParams;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::common::CommonArch;
use crate::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct OpenvmmHclBaselineOutput {
    #[serde(rename = "openhcl")]
    pub bin: PathBuf,
}

impl Artifact for OpenvmmHclBaselineOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub baseline: WriteVar<OpenvmmHclBaselineOutput>,
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
        let Request { target, baseline } = request;

        let recipe = match target.common_arch().unwrap() {
            CommonArch::X86_64 => OpenhclIgvmRecipe::X64,
            CommonArch::Aarch64 => OpenhclIgvmRecipe::Aarch64,
        }
        .recipe_details(true);

        let baseline_hcl_build = ctx.reqv(|v| build_openvmm_hcl::Request {
            build_params: OpenvmmHclBuildParams {
                target,
                profile: OpenvmmHclBuildProfile::OpenvmmHclShip,
                features: recipe.openvmm_hcl_features,
                no_split_dbg_info: false,
                max_trace_level: recipe.max_trace_level,
            },
            openvmm_hcl_output: v,
        });

        baseline_hcl_build
            .write_into_with(ctx, baseline, |b| OpenvmmHclBaselineOutput { bin: b.bin });

        Ok(())
    }
}
