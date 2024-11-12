// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and publish the vmgs_lib artifact

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
        ctx.import::<crate::artifact_vmgs_lib::publish::Node>();
        ctx.import::<crate::build_and_test_vmgs_lib::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            profile,
            artifact_dir,
            done,
        } = request;

        let vmgs_lib = ctx.reqv(|v| crate::build_and_test_vmgs_lib::Request {
            profile,
            target,
            vmgs_lib: v,
        });

        ctx.req(crate::artifact_vmgs_lib::publish::Request {
            vmgs_lib,
            artifact_dir,
            done,
        });

        Ok(())
    }
}
