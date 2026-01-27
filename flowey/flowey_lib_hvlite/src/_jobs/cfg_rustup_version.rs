// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;

pub const RUSTUP_TOOLCHAIN: &str = "1.91.1";

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_rust::Node>();
    }

    fn process_request(_: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        ctx.req(flowey_lib_common::install_rust::Request::Version(
            RUSTUP_TOOLCHAIN.into(),
        ));
        Ok(())
    }
}

flowey_request! {
    pub enum Request {
        Init,
    }
}
