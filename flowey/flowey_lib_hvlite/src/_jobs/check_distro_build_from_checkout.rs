// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assemble the OpenVMM source archive and run the distribution-build gate in
//! a single job.
//!
//! The release pipeline assembles the archive in a dedicated job and passes it
//! onward as an artifact, so its gate job receives a pre-built archive.
//! Ordinary pull-request CI has no release preparation job, so it must
//! assemble its own snapshot of the commit under test. A pipeline job can only
//! name root nodes, not wire variables between them, so that hand-off lives
//! here. Both entry points then run the same build via `check_distro_build`,
//! which keeps PR CI honest: it exercises the code path a release actually
//! uses.

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
        ctx.import::<crate::assemble_openvmm_source_release::Node>();
        ctx.import::<crate::_jobs::check_distro_build::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { done } = request;

        let release =
            ctx.reqv(|release| crate::assemble_openvmm_source_release::Request { release });

        ctx.req(crate::_jobs::check_distro_build::Request { release, done });

        Ok(())
    }
}
