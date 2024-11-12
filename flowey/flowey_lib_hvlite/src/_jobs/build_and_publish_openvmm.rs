// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds and publishes an OpenVMM binary artifact

use crate::build_openvmm::OpenvmmFeature;
use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use std::collections::BTreeSet;

flowey_request! {
    pub struct Params {
        pub profile: CommonProfile,
        pub target: CommonTriple,
        pub features: BTreeSet<OpenvmmFeature>,
        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::artifact_openvmm::publish::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            profile,
            target,
            features,
            artifact_dir,
            done,
        } = request;

        let openvmm = ctx.reqv(|v| crate::build_openvmm::Request {
            params: crate::build_openvmm::OpenvmmBuildParams {
                profile,
                target: target.clone(),
                features: features.clone(),
            },
            openvmm: v,
        });

        ctx.req(crate::artifact_openvmm::publish::Request {
            openvmm,
            artifact_dir,
            done,
        });

        Ok(())
    }
}
