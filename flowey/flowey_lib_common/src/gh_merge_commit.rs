// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub openvmm_repo_path: ReadVar<PathBuf>,
        pub merge_commit: WriteVar<String>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            openvmm_repo_path,
            merge_commit,
        } = request;

        ctx.emit_rust_step("get merge commit", |ctx| {
            let repo_path = openvmm_repo_path.claim(ctx);
            let merge_commit = merge_commit.claim(ctx);

            |rt| {
                let repo_path = rt.read(repo_path);
                let sh = xshell::Shell::new()?;
                sh.change_dir(repo_path);

                // TODO: Make this work for non-main PRs
                xshell::cmd!(sh, "git fetch origin main").run()?;
                let output = xshell::cmd!(sh, "git merge-base HEAD origin/main").read();

                if let Ok(commit) = output {
                    rt.write(merge_commit, &commit);
                } else {
                    anyhow::bail!("Failed to get action id");
                }

                Ok(())
            }
        });

        Ok(())
    }
}
