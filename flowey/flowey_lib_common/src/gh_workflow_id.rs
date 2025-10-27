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

/// Common parameters for all workflow queries
#[derive(Serialize, Deserialize)]
pub struct WorkflowQueryParams {
    pub github_commit_hash: ReadVar<String>,
    pub repo_path: ReadVar<PathBuf>,
    pub pipeline_name: String,
    pub gh_workflow: WriteVar<GithubWorkflow>,
}

/// Basic workflow query with default settings
#[derive(Serialize, Deserialize)]
pub struct BasicQuery {
    #[serde(flatten)]
    pub params: WorkflowQueryParams,
}

/// Query with custom status and specific job name
#[derive(Serialize, Deserialize)]
pub struct QueryWithStatusAndJob {
    #[serde(flatten)]
    pub params: WorkflowQueryParams,
    pub gh_run_status: GhRunStatus,
    pub gh_run_job_name: String,
}

flowey_request! {
    pub enum Request {
        /// Get workflow ID with default settings (success status)
        Basic(BasicQuery),
        /// Get workflow ID with custom status and specific job name
        WithStatusAndJob(QueryWithStatusAndJob),
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
            let (params, gh_run_status, gh_run_job_name) = match request {
                Request::Basic(BasicQuery { params }) => (params, GhRunStatus::Success, None),
                Request::WithStatusAndJob(QueryWithStatusAndJob {
                    params,
                    gh_run_status,
                    gh_run_job_name,
                }) => (params, gh_run_status, Some(gh_run_job_name)),
            };

            let WorkflowQueryParams {
                github_commit_hash,
                repo_path,
                pipeline_name,
                gh_workflow,
            } = params;

            let pipeline_name = pipeline_name.clone();
            let gh_cli = ctx.reqv(crate::use_gh_cli::Request::Get);

            ctx.emit_rust_step("get action id by commit", |ctx| {
                let gh_workflow = gh_workflow.claim(ctx);
                let github_commit_hash = github_commit_hash.claim(ctx);
                let repo_path = repo_path.claim(ctx);
                let pipeline_name = pipeline_name.clone();
                let gh_cli = gh_cli.claim(ctx);

                move |rt| {
                    let mut github_commit_hash = rt.read(github_commit_hash);
                    let sh = xshell::Shell::new()?;
                    let repo_path = rt.read(repo_path);
                    let gh_cli = rt.read(gh_cli);
                    let gh_run_status = match gh_run_status {
                        GhRunStatus::Completed => "completed",
                        GhRunStatus::Success => "success",
                    };

                    sh.change_dir(repo_path);

                    let handle_output = |output: Result<String, xshell::Error>, error_msg: &str| -> Option<String> {
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
                        let output = xshell::cmd!(
                            sh,
                            "{gh_cli} run list
                            --commit {commit}
                            -w {pipeline_name}
                            -s {gh_run_status}
                            -L 1
                            --json databaseId
                            --jq .[].databaseId"
                        )
                        .read();

                        handle_output(output, &format!("Failed to get action id for commit {}", commit))
                    };

                    // Verify a job with a given name and status exists for an action id
                    let verify_job_exists = |action_id: &str, job_name: &str| -> Option<String> {
                        // cmd! will escape quotes in any strings passed as an arg. Since we need multiple layers of
                        // escapes, first create the jq filter and then let cmd! handle the escaping.
                        let select = format!(".jobs[] | select(.name == \"{job_name}\" and .conclusion == \"success\") | .url");
                        let output = xshell::cmd!(
                            sh,
                            "{gh_cli} run view {action_id}
                            --json jobs
                            --jq={select}"
                        )
                        .read();

                        handle_output(output, &format!("Failed to get job {} for action id {}", job_name, action_id))
                    };

                    // Closure to get action id for a commit, with optional job verification
                    let get_action_id = |commit: String| -> Option<String> {
                        let action_id = get_action_id_for_commit(&commit)?;

                        // If a specific job name is required, verify the job exists with correct status
                        if let Some(job_name) = &gh_run_job_name {
                            verify_job_exists(&action_id, job_name)?;
                        }

                        Some(action_id)
                    };

                    let mut action_id = get_action_id(github_commit_hash.clone());
                    let mut loop_count = 0;

                    // CI may not have finished the build for the merge base, so loop through commits
                    // until we find a finished build or fail after 5 attempts
                    while action_id.is_none() {
                        println!(
                            "Unable to get action id for commit {}, trying again",
                            github_commit_hash
                        );

                        if loop_count > 4 {
                            anyhow::bail!("Failed to get action id after 5 attempts");
                        }

                        github_commit_hash =
                            xshell::cmd!(sh, "git rev-parse {github_commit_hash}^").read()?;
                        action_id = get_action_id(github_commit_hash.clone());

                        loop_count += 1;
                    }

                    // We have an action id or we would've bailed in the loop above
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
        }

        Ok(())
    }
}
