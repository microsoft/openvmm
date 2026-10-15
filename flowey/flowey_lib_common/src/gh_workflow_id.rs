// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Gets the Github workflow id for a given commit hash

use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub enum GhRunStatus {
    Completed,
    Success,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GithubWorkflow {
    pub id: String,
    pub commit: String,
}

#[derive(Serialize, Deserialize)]
pub enum GitCommitOrBranch {
    Commit(ReadVar<String>),
    Branch(ReadVar<String>),
}

flowey_request! {
    pub struct Request {
        /// First component of a github repo path
        pub repo_owner: String,
        /// Second component of a github repo path
        pub repo_name: String,
        /// Commit hash or branch name
        pub commit_or_branch: GitCommitOrBranch,
        /// Pipeline name (the .yaml file)
        pub pipeline_name: String,
        /// Require that the run have a certain status
        pub require_run_status: Option<GhRunStatus>,
        /// Require that a certain job within the run be successful
        pub require_successful_job_with_name: Option<String>,
        /// Output workflow id and associated commit hash
        pub gh_workflow: WriteVar<GithubWorkflow>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::use_gh_cli::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        for request in requests {
            let Request {
                repo_owner,
                repo_name,
                commit_or_branch,
                pipeline_name,
                require_run_status,
                require_successful_job_with_name,
                gh_workflow,
            } = request;

            let pipeline_name = pipeline_name.clone();
            let gh_cli = ctx.reqv(crate::use_gh_cli::Request::Get);

            let repo = format!("{repo_owner}/{repo_name}");
            let commit_hash = match commit_or_branch {
                GitCommitOrBranch::Commit(commit) => commit,
                GitCommitOrBranch::Branch(branch) => {
                    ctx.emit_rust_stepv("get latest commit by branch", |ctx| {
                        let branch = branch.claim(ctx);
                        let gh_cli = gh_cli.clone().claim(ctx);
                        let repo = repo.clone();

                        move |rt| {
                            let branch = rt.read(branch);
                            let gh_cli = rt.read(gh_cli);

                            let commit_hash = flowey::shell_cmd!(
                                rt,
                                "{gh_cli} api repos/{repo}/commits/{branch} --jq .sha"
                            )
                            .read()?
                            .trim()
                            .to_string();
                            Ok(commit_hash)
                        }
                    })
                }
            };

            ctx.emit_rust_step("get action id by commit", |ctx| {
                let gh_workflow = gh_workflow.claim(ctx);
                let commit_hash = commit_hash.claim(ctx);
                let pipeline_name = pipeline_name.clone();
                let gh_cli = gh_cli.claim(ctx);

                move |rt| {
                    let commit_hash = rt.read(commit_hash);
                    let gh_cli = rt.read(gh_cli);

                    let workflow = get_action_id_by_commit(
                        rt,
                        commit_hash,
                        gh_cli,
                        repo,
                        pipeline_name,
                        require_run_status,
                        require_successful_job_with_name,
                    )?;

                    println!("Got action id {}, commit {}", workflow.id, workflow.commit);
                    rt.write(gh_workflow, &workflow);

                    Ok(())
                }
            });
        }

        Ok(())
    }
}

fn get_action_id_by_commit(
    rt: &mut RustRuntimeServices<'_>,
    mut commit_hash: String,
    gh_cli: PathBuf,
    repo: String,
    pipeline_name: String,
    require_run_status: Option<GhRunStatus>,
    require_successful_job_with_name: Option<String>,
) -> anyhow::Result<GithubWorkflow> {
    let (run_status_flag, run_status_value) = require_run_status
        .map(|s| {
            (
                "-s",
                match s {
                    GhRunStatus::Completed => "completed",
                    GhRunStatus::Success => "success",
                },
            )
        })
        .unzip();

    let handle_output =
        |output: Result<String, xshell::Error>, error_msg: &str| -> Option<String> {
            match output {
                Ok(output) if output.trim().is_empty() => None,
                Ok(output) => Some(output.trim().to_string()),
                Err(e) => {
                    println!("{}: {}", error_msg, e);
                    None
                }
            }
        };

    // Get action id for a specific commit
    let get_action_id_for_commit = |commit: &str| -> Option<String> {
        let output = flowey::shell_cmd!(
            rt,
            "{gh_cli} run list
            -R {repo}
            --commit {commit}
            -w {pipeline_name}
            {run_status_flag...} {run_status_value...}
            -L 1
            --json databaseId
            --jq .[].databaseId"
        )
        .read();

        handle_output(
            output,
            &format!("Failed to get action id for commit {}", commit),
        )
    };

    // Verify a job with a given name and status exists for an action id
    let verify_job_exists = |action_id: &str, job_name: &str| -> Option<String> {
        // cmd! will escape quotes in any strings passed as an arg. Since we need multiple layers of
        // escapes, first create the jq filter and then let cmd! handle the escaping.
        let select = format!(
            ".jobs[] | select(.name == \"{job_name}\" and .conclusion == \"success\") | .url"
        );
        let output = flowey::shell_cmd!(
            rt,
            "{gh_cli} run view {action_id}
            -R {repo}
            --json jobs
            --jq={select}"
        )
        .read();

        handle_output(
            output,
            &format!("Failed to get job {} for action id {}", job_name, action_id),
        )
    };

    // Closure to get action id for a commit, with optional job verification
    let get_action_id = |commit: &str| -> Option<String> {
        let action_id = get_action_id_for_commit(commit)?;

        // If a specific job name is required, verify the job exists with correct status
        if let Some(job_name) = &require_successful_job_with_name {
            verify_job_exists(&action_id, job_name)?;
        }

        Some(action_id)
    };

    let mut action_id = get_action_id(&commit_hash);
    let mut loop_count = 0;

    // CI may not have finished the build for the merge base, so loop through commits
    // until we find a finished build or fail after 5 attempts
    while action_id.is_none() {
        println!(
            "Unable to get action id for commit {}, trying again",
            commit_hash
        );

        if loop_count > 4 {
            anyhow::bail!("Failed to get action id after 5 attempts");
        }

        commit_hash = flowey::shell_cmd!(
            rt,
            "{gh_cli} api repos/{repo}/commits/{commit_hash} --jq .parents[0].sha"
        )
        .read()?
        .trim()
        .to_string();
        action_id = get_action_id(&commit_hash);

        loop_count += 1;
    }

    // We have an action id or we would've bailed in the loop above
    let id = action_id.context("failed to get action id")?;

    Ok(GithubWorkflow {
        id,
        commit: commit_hash,
    })
}
