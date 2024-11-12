// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and publish rustdocs for the HvLite repo

use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub target_triple: target_lexicon::Triple,

        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_rustdoc::publish::Node>();
        ctx.import::<crate::build_rustdoc::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target_triple,
            artifact_dir,
            done,
        } = request;

        let rustdocs_dir = ctx.reqv(|v| crate::build_rustdoc::Request::Doc {
            target_triple,
            docs_dir: v,
        });

        ctx.req(crate::artifact_rustdoc::publish::Request {
            rustdocs_dir,
            artifact_dir,
            done,
        });

        Ok(())
    }
}
