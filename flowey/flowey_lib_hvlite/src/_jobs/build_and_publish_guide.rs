// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and publish the OpenVMM Guide using `mdbook`

use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::download_mdbook::Node>();
        ctx.import::<crate::artifact_guide::publish::Node>();
        ctx.import::<crate::build_guide::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params { artifact_dir, done } = request;

        let rendered_guide = ctx.reqv(|v| crate::build_guide::Request { built_guide: v });

        ctx.req(crate::artifact_guide::publish::Request {
            rendered_guide: rendered_guide.clone(),
            artifact_dir,
            done,
        });

        Ok(())
    }
}
