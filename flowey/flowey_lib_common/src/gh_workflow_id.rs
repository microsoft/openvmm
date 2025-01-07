// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub github_commit_hash: ReadVar<String>,
        pub gh_workflow_id: WriteVar<String>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            github_commit_hash,
            gh_workflow_id,
        } = request;

        let gh_token = ctx.get_gh_context_var(GhContextVar::GITHUB__TOKEN);

        ctx.emit_rust_step("get action id", |ctx| {
            let gh_workflow_id = gh_workflow_id.claim(ctx);
            let github_commit_hash = github_commit_hash.claim(ctx);
            let gh_token = gh_token.claim(ctx);

            |rt| {
                let github_commit_hash = rt.read(github_commit_hash);
            let sh = xshell::Shell::new()?;
            let gh_token = rt.read(gh_token);
            // Fetches the CI build workflow id for a given commit hash
            let get_action_id = |commit: String| {
            xshell::cmd!(
                    sh,
                    "gh run list --commit {commit} -w '[flowey] OpenVMM CI' -s 'completed' -L 1 --json databaseId --jq '.[].databaseId'"
                )
                .env("GITHUB_TOKEN", gh_token)
                .read()
            };

            let action_id = get_action_id(github_commit_hash);

            if let Ok(id) = action_id {
                print!("Got action id: {}", id);
                rt.write(gh_workflow_id, &id);
            } else {
                anyhow::bail!("Failed to get action id");
            }

            Ok(())
        }
        });

        Ok(())
    }
}
