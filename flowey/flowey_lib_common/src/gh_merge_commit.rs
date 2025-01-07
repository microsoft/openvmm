// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub repo_path: ReadVar<PathBuf>,
        pub merge_commit: WriteVar<String>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            repo_path,
            merge_commit,
        } = request;

        let head_ref = ctx.get_gh_context_var(GhContextVar::GITHUB__HEAD_REF);
        let pr_number = ctx.get_gh_context_var(GhContextVar::GITHUB__PR_NUMBER);

        ctx.emit_rust_step("get merge commit", |ctx| {
            let merge_commit = merge_commit.claim(ctx);
            let head_ref = head_ref.claim(ctx);
            let repo_path = repo_path.claim(ctx);
            let pr_number = pr_number.claim(ctx);

            |rt| {
                let sh = xshell::Shell::new()?;
                let repo_path = rt.read(repo_path);
                let head_ref = rt.read(head_ref);
                let pr_number = rt.read(pr_number);

                sh.change_dir(repo_path);

                // TODO: Make this work for non-main PRs
                xshell::cmd!(sh, "git fetch origin pull/{pr_number}/head:{head_ref}").run()?;
                let commit = xshell::cmd!(sh, "git merge-base {head_ref} origin/main").read()?;
                rt.write(merge_commit, &commit);

                Ok(())
            }
        });

        Ok(())
    }
}
