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

        ctx.emit_rust_step("get merge commit", move |ctx| {
            let merge_commit = merge_commit.claim(ctx);
            let repo_path = repo_path.claim(ctx);

            move |rt| {
                let repo_path = rt.read(repo_path);

                rt.sh.change_dir(repo_path);

                flowey::shell_cmd!(rt, "git fetch --unshallow").run()?;

                flowey::shell_cmd!(rt, "git fetch origin {base_branch}").run()?;
                let commit =
                    flowey::shell_cmd!(rt, "git merge-base HEAD origin/{base_branch}").read()?;
                rt.write(merge_commit, &commit);

                Ok(())
            }
        });

        Ok(())
    }
}
