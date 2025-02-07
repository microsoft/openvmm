// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Gets the Github workflow id for a given commit hash

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub github_commit_hash: ReadVar<String>,
        pub repo_path: ReadVar<PathBuf>,
        pub pipeline_name: String,
        pub gh_token: ReadVar<String>,
        pub gh_workflow: WriteVar<GithubWorkflow>,
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GithubWorkflow {
    pub id: String,
    pub commit: String,
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
            github_commit_hash,
            gh_workflow,
            pipeline_name,
            gh_token,
        } = request;

        let pipeline_name = pipeline_name.clone();

        ctx.req(crate::use_gh_cli::Request::WithAuth(
            crate::use_gh_cli::GhCliAuth::AuthToken(gh_token.clone()),
        ));
        let gh_cli = ctx.reqv(crate::use_gh_cli::Request::Get);

        ctx.emit_rust_step("get action id", |ctx| {
            let gh_workflow = gh_workflow.claim(ctx);
            let github_commit_hash = github_commit_hash.claim(ctx);
            let repo_path = repo_path.claim(ctx);
            let pipeline_name = pipeline_name.clone();
            let gh_cli = gh_cli.claim(ctx);

            move |rt| {
                let github_commit_hash = rt.read(github_commit_hash);
                let sh = xshell::Shell::new()?;
                let repo_path = rt.read(repo_path);
                let gh_cli = rt.read(gh_cli);

                sh.change_dir(repo_path);

                // Fetches the CI build workflow id for a given commit hash
                let get_action_id = |commit: String| {
                    xshell::cmd!(
                        sh,
                        "{gh_cli} run list
                        --commit {commit}
                        -w {pipeline_name}
                        -s completed
                        -L 1
                        --json databaseId
                        --jq .[].databaseId"
                    )
                    .read()
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

                    github_commit_hash =
                        xshell::cmd!(sh, "git rev-parse {github_commit_hash}^").read()?;
                    action_id = get_action_id(github_commit_hash.clone());
                    loop_count += 1;
                }

                let id = action_id.context("failed to get action id")?;

                println!("Got action id {id}, commit {github_commit_hash}");
                rt.write(
                    gh_workflow,
                    &GithubWorkflow {
                        id,
                        commit: github_commit_hash,
                    },
                );

                Ok(())
            }
        });

        Ok(())
    }
}
