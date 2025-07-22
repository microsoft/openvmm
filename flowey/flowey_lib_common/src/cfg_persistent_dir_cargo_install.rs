// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A node which returns a PathBuf to a single shared persistent-dir that can be
//! used by any nodes invoking `cargo install` in order to share a single cargo
//! build cache.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request(pub WriteVar<Option<PathBuf>>);
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let persistent_dir = ctx.persistent_dir();
        ctx.emit_minor_rust_step("report cargo install persistent dir", |ctx| {
            let persistent_dir = persistent_dir.claim(ctx);
            let requests = requests
                .into_iter()
                .map(|x| x.0.claim(ctx))
                .collect::<Vec<_>>();
            |rt| {
                let persistent_dir = rt.read(persistent_dir);
                for var in requests {
                    rt.write(var, &persistent_dir)
                }
            }
        });

        Ok(())
    }
}
