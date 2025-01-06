// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub merge_commit: WriteVar<String>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { merge_commit } = request;

        let head_ref = ctx.get_gh_context_var(GhContextVar::GITHUB__HEAD_REF);

        ctx.emit_rust_step("get merge commit", |ctx| {
            let merge_commit = merge_commit.claim(ctx);
            let head_ref = head_ref.claim(ctx);

            |rt| {
                let sh = xshell::Shell::new()?;
                let head_ref = rt.read(head_ref);

                // TODO: Make this work for non-main PRs
                let commit = xshell::cmd!(sh, "git merge-base {head_ref} origin/main").read()?;
                rt.write(merge_commit, &commit);

                Ok(())
            }
        });

        Ok(())
    }
}
