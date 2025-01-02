// Copyright (C) Microsoft Corporation. All rights reserved.

use flowey::node::prelude::*;
use crate::build_openvmm::OpenvmmOutput;

flowey_request! {
    pub struct Request {
        pub old_openvmm: ReadVar<OpenvmmOutput>,
        pub new_openvmm: ReadVar<OpenvmmOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            old_openvmm,
            new_openvmm,
        } = request;

        let old_openvmm = old_openvmm.claim(ctx);
        let new_openvmm = new_openvmm.claim(ctx);

        Ok(())
    }
}
