// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assemble the OpenVMM vendor archive and run the distribution-build gate in
//! a single job.
//!
//! The release pipeline assembles the archive in a dedicated job and hands it
//! to the gate as an artifact. PR CI has no such job, so it assembles its own
//! snapshot of the commit under test. A pipeline job can only name root nodes,
//! not wire variables between them, so that hand-off lives here.

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
        ctx.import::<crate::assemble_openvmm_vendor_release::Node>();
        ctx.import::<crate::_jobs::check_distro_build::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { done } = request;

        let release =
            ctx.reqv(|release| crate::assemble_openvmm_vendor_release::Request { release });

        ctx.req(crate::_jobs::check_distro_build::Request { release, done });

        Ok(())
    }
}
