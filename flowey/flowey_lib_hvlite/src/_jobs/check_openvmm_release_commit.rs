// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Gate a source release on the requested revision being reviewed.
//!
//! This job never checks out the requested revision, so it runs no code from
//! it. Every job that does is ordered after this one, and this job is also the
//! one that bootstraps the Flowey the write-capable publish job later
//! consumes.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::verify_openvmm_release_commit::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { done } = request;

        let revision = ctx
            .get_gh_context_var()
            .event()
            .repository_dispatch_revision();

        ctx.req(crate::verify_openvmm_release_commit::Request { revision, done });

        Ok(())
    }
}
