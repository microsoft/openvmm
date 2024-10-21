// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and publish the vmgstool artifact

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub with_crypto: bool,

        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_vmgstool::publish::Node>();
        ctx.import::<crate::build_vmgstool::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            profile,
            with_crypto,
            artifact_dir,
            done,
        } = request;

        let vmgstool = ctx.reqv(|v| crate::build_vmgstool::Request {
            profile,
            target,
            with_crypto,
            vmgstool: v,
        });

        ctx.req(crate::artifact_vmgstool::publish::Request {
            vmgstool,
            artifact_dir,
            done,
        });

        Ok(())
    }
}
