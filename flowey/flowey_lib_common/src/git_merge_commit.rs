// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Gets the merge commit of a PR to base branch

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub repo_path: ReadVar<PathBuf>,
        pub merge_commit: WriteVar<String>,
        pub base_branch: String,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_git::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            repo_path,
            merge_commit,
            base_branch,
        } = request;

        let pr_event = ctx.get_gh_context_var().event().pull_request();

        ctx.emit_rust_step("get merge commit", move |ctx| {
            let merge_commit = merge_commit.claim(ctx);
            let pr_event = pr_event.claim(ctx);
            let repo_path = repo_path.claim(ctx);

            move |rt| {
                let sh = xshell::Shell::new()?;
                let repo_path = rt.read(repo_path);
                let pr_event = rt.read(pr_event).expect("PR event not found");

                let head_ref = pr_event.head.head_ref;
                let pr_number = pr_event.number.to_string();

                sh.change_dir(repo_path);

                xshell::cmd!(sh, "git fetch origin {base_branch}").run()?;
                xshell::cmd!(sh, "git fetch origin pull/{pr_number}/head:{head_ref}").run()?;
                let commit =
                    xshell::cmd!(sh, "git merge-base {head_ref} origin/{base_branch}").read()?;
                rt.write(merge_commit, &commit);

                Ok(())
            }
        });

        Ok(())
    }
}
