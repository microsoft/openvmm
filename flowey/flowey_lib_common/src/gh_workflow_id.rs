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

        ctx.emit_rust_step("get action id", |ctx| {
            let gh_workflow_id = gh_workflow_id.claim(ctx);
            let github_commit_hash = github_commit_hash.claim(ctx);

            |rt| {
                let github_commit_hash = rt.read(github_commit_hash);
            let sh = xshell::Shell::new()?;
            // Fetches the CI build workflow id for a given commit hash
            let get_action_id = |commit: String| {
            xshell::cmd!(
                    sh,
                    "gh run list --commit {commit} -w '[flowey] OpenVMM CI' -s 'completed' -L 1 --json databaseId --jq '.[].databaseId'"
                )
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
