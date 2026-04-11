// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A minimal "hello world" job node used to validate that a CI pool and image
//! are reachable and functional.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params { done } = request;

        ctx.emit_rust_step("Hello, World!", |ctx| {
            done.claim(ctx);
            |_rt| {
                log::info!("Hello, World! The CI image is reachable and functional.");
                Ok(())
            }
        });

        Ok(())
    }
}
