// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Gets the Github workflow id for a given commit hash

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub github_commit_hash: ReadVar<String>,
        pub repo_path: ReadVar<PathBuf>,
        pub gh_workflow_id: WriteVar<String>,
        pub pipeline_name: String,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            repo_path,
            github_commit_hash,
            gh_workflow_id,
            pipeline_name,
        } = request;

        let gh_token = ctx.get_gh_context_var().global().token();
        let pipeline_name = pipeline_name.clone();

        ctx.emit_rust_step("get action id", |ctx| {
            let gh_workflow_id = gh_workflow_id.claim(ctx);
            let github_commit_hash = github_commit_hash.claim(ctx);
            let gh_token = gh_token.claim(ctx);
            let repo_path = repo_path.claim(ctx);
            let pipeline_name = pipeline_name.clone();

            move |rt| {
                let github_commit_hash = rt.read(github_commit_hash);
            let sh = xshell::Shell::new()?;
            let gh_token = rt.read(gh_token);
            let repo_path = rt.read(repo_path);

            sh.change_dir(repo_path);
            // Fetches the CI build workflow id for a given commit hash

            let get_action_id = |commit: String| {
            let cmd = format!("gh run list --commit {} -w '{}' -s 'completed' -L 1 --json databaseId --jq '.[].databaseId'", commit, pipeline_name);
            sh.cmd(cmd).env("GITHUB_TOKEN", gh_token.clone()).read()
            };

            let mut github_commit_hash = github_commit_hash.clone();
            let mut action_id = get_action_id(github_commit_hash.clone());
            let mut loop_count = 0;

            // CI may not have finished the build for the merge base, so loop through commits
            // until we find a finished build or fail after 5 attempts
            while let Err(ref e) = action_id {
                if loop_count > 4 {
                    anyhow::bail!("Failed to get action id after 5 attempts: {}", e);
                }

                github_commit_hash = xshell::cmd!(sh, "git rev-parse {github_commit_hash}^").read()?;
                action_id = get_action_id(github_commit_hash.clone());
                loop_count += 1;
            }

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
