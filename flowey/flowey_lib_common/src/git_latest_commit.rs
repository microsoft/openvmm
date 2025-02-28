// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Gets the latest commit of a branch

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub repo_path: ReadVar<PathBuf>,
        pub branch: String,
        pub commit: WriteVar<String>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::use_gh_cli::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            repo_path,
            commit,
            branch,
        } = request;

        let gh_cli = ctx.reqv(crate::use_gh_cli::Request::Get);

        ctx.emit_rust_step("get latest commit", move |ctx| {
            let repo_path = repo_path.claim(ctx);
            let commit = commit.claim(ctx);
            let gh_cli = gh_cli.claim(ctx);

            move |rt| {
                let sh = xshell::Shell::new()?;
                let repo_path = rt.read(repo_path);
                let gh_cli = rt.read(gh_cli);

                sh.change_dir(repo_path);

                let latest_commit = xshell::cmd!(
                    sh,
                    "{gh_cli} api repos/microsoft/openvmm/commits/{branch} --jq .sha"
                )
                .read()?;
                rt.write(commit, &latest_commit);

                Ok(())
            }
        });

        Ok(())
    }
}
