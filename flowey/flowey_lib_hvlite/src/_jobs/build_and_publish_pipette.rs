// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and publish the pipette artifact

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub target: CommonTriple,
        pub profile: CommonProfile,

        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_pipette::publish::Node>();
        ctx.import::<crate::build_pipette::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            profile,
            artifact_dir,
            done,
        } = request;

        let pipette = ctx.reqv(|v| crate::build_pipette::Request {
            profile,
            target,
            pipette: v,
        });

        ctx.req(crate::artifact_pipette::publish::Request {
            pipette,
            artifact_dir,
            done,
        });

        Ok(())
    }
}
