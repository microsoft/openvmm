// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code for emitting a pipeline as a single self-contained GitHub Actions yaml file

use super::common_yaml::BashCommands;
use super::common_yaml::check_generated_yaml_and_json;
use super::common_yaml::write_generated_yaml_and_json;
use super::generic::ResolvedJobArtifact;
use super::generic::ResolvedJobUseParameter;
use crate::cli::exec_snippet::FloweyPipelineStaticDb;
use crate::cli::exec_snippet::VAR_DB_SEEDVAR_FLOWEY_WORKING_DIR;
use crate::cli::pipeline::CheckMode;
use crate::cli::var_db::VarDbRequestBuilder;
use crate::flow_resolver::stage1_dag::OutputGraphEntry;
use crate::flow_resolver::stage1_dag::Step;
use crate::pipeline_resolver::common_yaml::FloweySource;
use crate::pipeline_resolver::common_yaml::job_flowey_bootstrap_source;
use crate::pipeline_resolver::generic::ResolvedPipeline;
use crate::pipeline_resolver::generic::ResolvedPipelineJob;
use anyhow::Context;
use flowey_core::node::FlowArch;
use flowey_core::node::FlowBackend;
use flowey_core::node::FlowPlatform;
use flowey_core::node::FlowPlatformKind;
use flowey_core::node::NodeHandle;
use flowey_core::node::user_facing::GhPermission;
use flowey_core::node::user_facing::GhPermissionValue;
use flowey_core::pipeline::GhRunner;
use flowey_core::pipeline::GhRunnerOsLabel;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::Path;
mod github_yaml_defs;

const RUNNER_TEMP: &str = "${{ runner.temp }}";

/// Emit a pipeline as a single self-contained GitHub Actions yaml file
pub fn github_yaml(
    pipeline: ResolvedPipeline,
    runtime_debug_log: bool,
    repo_root: &Path,
    pipeline_file: &Path,
    flowey_crate: &str,
    check: CheckMode,
) -> anyhow::Result<()> {
    if pipeline_file.extension().and_then(|s| s.to_str()) != Some("yaml") {
        anyhow::bail!("pipeline name must end with .yaml")
    }

    let ResolvedPipeline {
        graph,
        order,
        gh_name,
        gh_schedule_triggers,
        gh_ci_triggers,
        gh_pr_triggers,
        gh_bootstrap_template,
        parameters,
        ado_name: _,
        ado_schedule_triggers: _,
        ado_ci_triggers: _,
        ado_pr_triggers: _,
        ado_bootstrap_template: _,
        ado_resources_repository: _,
        ado_post_process_yaml_cb: _,
        ado_variables: _,
        ado_job_id_overrides: _,
    } = pipeline;

    let mut job_flowey_source: BTreeMap<petgraph::prelude::NodeIndex, FloweySource> =
        job_flowey_bootstrap_source(&graph, &order);

    let mut pipeline_static_db = FloweyPipelineStaticDb {
        flow_backend: crate::cli::FlowBackendCli::Github,
        var_db_backend_kind: crate::cli::exec_snippet::VarDbBackendKind::Json,
        job_reqs: BTreeMap::new(),
    };

    let mut github_jobs = BTreeMap::new();

    for job_idx in order {
        let ResolvedPipelineJob {
            ref root_nodes,
            ref patches,
            ref label,
            platform,
            arch,
            ref external_read_vars,
            ado_pool: _,
            timeout_minutes,
            ref gh_override_if,
            ref gh_global_env,
            ref gh_pool,
            ref gh_permissions,
            cond_param_idx,
            ref parameters_used,
            ref artifacts_used,
            ref artifacts_published,
            ado_variables: _,
        } = graph[job_idx];

        if cond_param_idx.is_some() {
            anyhow::bail!(
                "conditional params are not supported in GitHub backend, use `gh_dangerous_override_if` instead"
            );
        }

        let flowey_bin = platform.binary("flowey");
        let (steps, req_db) = resolve_flow_as_github_yaml_steps(
            root_nodes
                .clone()
                .into_iter()
                .map(|(node, requests)| (node, (true, requests)))
                .collect(),
            patches.clone(),
            external_read_vars.clone(),
            platform,
            arch,
            job_idx.index(),
            &flowey_bin,
            gh_permissions,
        )
        .context(format!("in job '{label}'"))?;

        {
            let existing = pipeline_static_db.job_reqs.insert(job_idx.index(), req_db);
            assert!(existing.is_none())
        }

        let mut gh_steps = Vec::new();

        let flowey_source = job_flowey_source.remove(&job_idx).unwrap();

        let mut artifact_names = Vec::new();

        let flowey_path = match &flowey_source {
            FloweySource::Bootstrap { .. } => {
                let flowey_path = "bootstrapped-flowey".to_string();

                // actual artifact publish happens at the end of the job
                if gh_bootstrap_template.is_empty() {
                    anyhow::bail!(
                        "Did not specify flowey bootstrap template. Please provide one using `Pipeline::gh_set_flowey_bootstrap_template`"
                    )
                }

                let gh_bootstrap_template = gh_bootstrap_template
                    .replace("{{FLOWEY_BIN_EXTENSION}}", platform.exe_suffix())
                    .replace("{{FLOWEY_CRATE}}", flowey_crate)
                    .replace(
                        "{{FLOWEY_PIPELINE_PATH}}",
                        &pipeline_file.with_extension("").display().to_string(),
                    )
                    .replace(
                        "{{FLOWEY_TARGET}}",
                        match (platform, arch) {
                            (FlowPlatform::Windows, FlowArch::X86_64) => "x86_64-pc-windows-msvc",
                            (FlowPlatform::Windows, FlowArch::Aarch64) => "aarch64-pc-windows-msvc",
                            (FlowPlatform::Linux(_), FlowArch::X86_64) => {
                                "x86_64-unknown-linux-gnu"
                            }
                            (FlowPlatform::Linux(_), FlowArch::Aarch64) => {
                                "aarch64-unknown-linux-gnu"
                            }
                            (platform, arch) => {
                                anyhow::bail!("unsupported platform {platform} / arch {arch}")
                            }
                        },
                    )
                    .replace("{{FLOWEY_OUTDIR}}", &format!("{RUNNER_TEMP}/{flowey_path}"));

                let bootstrap_steps: serde_yaml::Sequence =
                    serde_yaml::from_str(&gh_bootstrap_template)
                        .context("malformed flowey bootstrap template")?;

                gh_steps.extend(bootstrap_steps);
                flowey_path
            }
            FloweySource::Consume(artifact) => {
                // download previously bootstrapped flowey
                artifact_names.push(artifact.as_str());
                format!("used_artifacts/{artifact}")
            }
        };

        // download any artifacts that'll be used
        artifact_names.extend(artifacts_used.iter().map(|a| a.name.as_str()));
        if !artifact_names.is_empty() {
            let pattern = if let &[name] = artifact_names.as_slice() {
                name.to_string()
            } else {
                format!("{{{}}}", artifact_names.join(","))
            };
            gh_steps.push({
                let map: serde_yaml::Mapping = serde_yaml::from_str(&format!(
                    r#"
                        name: '🌼📦 Download artifacts'
                        uses: actions/download-artifact@v4
                        with:
                          pattern: '{pattern}'
                          path: {RUNNER_TEMP}/used_artifacts/
                    "#
                ))
                .unwrap();
                map.into()
            });
        }

        {
            let mut map = serde_yaml::Mapping::new();
            map.insert(
                "run".into(),
                format!(r#"echo "{RUNNER_TEMP}/{flowey_path}" >> $GITHUB_PATH"#).into(),
            );
            map.insert("shell".into(), "bash".into());
            map.insert("name".into(), "🌼📦 Add flowey to PATH".into());
            gh_steps.push(map.into());
        }

        let var_db = VarDbRequestBuilder::new(&flowey_bin, job_idx.index());

        let bootstrap_bash_var_db_inject = |var, is_raw_string| {
            var_db
                .update_from_stdin(var, false)
                .raw_string(is_raw_string)
                .to_string()
        };

        // if this was a bootstrap job, also take a moment to run a "self check"
        // to make sure that the current checked-in template matches the one it
        // expected
        if let FloweySource::Bootstrap(..) = &flowey_source {
            let mut current_invocation = std::env::args().collect::<Vec<_>>();

            current_invocation[0] = flowey_bin.clone();

            // if this code path is run while generating the YAML to compare the
            // check against, we want to remove the --runtime or --check param from the
            // current call, or else there'll be a dupe
            let mut strip_parameter = |prefix: &str| {
                if let Some(i) = current_invocation
                    .iter()
                    .position(|s| s.starts_with(prefix))
                {
                    current_invocation.remove(i);
                    if !current_invocation[i].starts_with(prefix) {
                        current_invocation.remove(i);
                    }
                }
            };

            strip_parameter("--runtime");
            strip_parameter("--check");

            // insert the --check bit of the call alongside the --out param
            {
                let i = current_invocation
                    .iter()
                    .position(|s| s.starts_with("--out"))
                    .unwrap();

                let current_yaml = match platform.kind() {
                    FlowPlatformKind::Windows => {
                        let win_path = flowey_path.replace('/', "\\");
                        format!(r#"$ESCAPED_AGENT_TEMPDIR\\{win_path}\\pipeline.yaml"#)
                    }
                    FlowPlatformKind::Unix => {
                        format!(r#"$ESCAPED_AGENT_TEMPDIR/{flowey_path}/pipeline.yaml"#)
                    }
                };

                current_invocation.insert(i, current_yaml);
                current_invocation.insert(i, "--runtime".into());
            }

            // Need to use an escaped version of the "true" windows/linux path
            // here, or else the --check will fail.
            let cmd = format!(
                r###"
ESCAPED_AGENT_TEMPDIR=$(
cat <<'EOF' | sed 's/\\/\\\\/g'
{RUNNER_TEMP}
EOF
)
{}
"###,
                current_invocation.join(" ")
            );

            gh_steps.push({
                let mut map = serde_yaml::Mapping::new();
                map.insert("name".into(), "🌼🔎 Self-check YAML".into());
                map.insert(
                    "run".into(),
                    serde_yaml::Value::String(cmd.trim().to_string()),
                );
                map.insert("shell".into(), "bash".into());
                map.into()
            })
        }

        let mut flowey_bootstrap_bash = String::new();

        // and now use those vars to do some flowey bootstrap
        writeln!(flowey_bootstrap_bash, "{}", {
            let runtime_debug_level = if runtime_debug_log { "debug" } else { "info" };

            let var_db_insert_runtime_debug_level =
                bootstrap_bash_var_db_inject("FLOWEY_LOG", false);
            let var_db_insert_working_dir =
                bootstrap_bash_var_db_inject(VAR_DB_SEEDVAR_FLOWEY_WORKING_DIR, true);

            // Need to use "normalized" path in cases where the path is being
            // used directly from a bash context, as is the case when we are
            // trying to invoke `flowey.exe` in argv0 position)
            //
            // https://github.com/microsoft/azure-pipelines-tasks/issues/10653#issuecomment-585669089
            format!(
                r###"
AgentTempDirNormal="{RUNNER_TEMP}"
AgentTempDirNormal=$(echo "$AgentTempDirNormal" | sed -e 's|\\|\/|g' -e 's|^\([A-Za-z]\)\:/\(.*\)|/\L\1\E/\2|')
echo "AgentTempDirNormal=$AgentTempDirNormal" >> $GITHUB_ENV

chmod +x $AgentTempDirNormal/{flowey_path}/{flowey_bin}

echo '"{runtime_debug_level}"' | {var_db_insert_runtime_debug_level}
echo "{RUNNER_TEMP}/work" | {var_db_insert_working_dir}
"###
            )
            .trim_start()
            .to_owned()
        })?;

        // import pipeline vars being used by the job into flowey
        for ResolvedJobUseParameter {
            flowey_var,
            pipeline_param_idx,
        } in parameters_used
        {
            let is_string = matches!(
                parameters[*pipeline_param_idx],
                flowey_core::pipeline::internal::Parameter::String { .. }
            );

            let default = match &parameters[*pipeline_param_idx] {
                flowey_core::pipeline::internal::Parameter::Bool { default, .. } => {
                    default.map(|b| b.to_string())
                }
                flowey_core::pipeline::internal::Parameter::String { default, .. } => {
                    default.clone()
                }
                flowey_core::pipeline::internal::Parameter::Num { default, .. } => {
                    default.map(|n| n.to_string())
                }
            }
            .expect("defaults are currently required for parameters in Github backend");

            let var_db_inject_cmd = bootstrap_bash_var_db_inject(flowey_var, is_string);

            let name = parameters[*pipeline_param_idx].name();

            let cmd = format!(
                r#"
cat <<'EOF' | {var_db_inject_cmd}
${{{{ inputs.{name} != '' && inputs.{name} || '{default}' }}}}
EOF
"#
            )
            .trim()
            .to_string();
            writeln!(flowey_bootstrap_bash, "{}", cmd)?;
        }

        // next, emit GitHub steps to create dirs for artifacts which will be
        // published
        for ResolvedJobArtifact { flowey_var, name } in artifacts_published {
            writeln!(
                flowey_bootstrap_bash,
                r#"mkdir -p "$AgentTempDirNormal/publish_artifacts/{name}""#
            )?;
            let var_db_inject_cmd = bootstrap_bash_var_db_inject(flowey_var, true);
            match platform.kind() {
                FlowPlatformKind::Windows => {
                    writeln!(
                        flowey_bootstrap_bash,
                        r#"echo "{RUNNER_TEMP}\\publish_artifacts\\{name}" | {var_db_inject_cmd}"#,
                    )?;
                }
                FlowPlatformKind::Unix => {
                    writeln!(
                        flowey_bootstrap_bash,
                        r#"echo "$AgentTempDirNormal/publish_artifacts/{name}" | {var_db_inject_cmd}"#,
                    )?;
                }
            }
        }

        // lastly, emit GitHub steps that report the dirs for any artifacts which
        // are used by this job
        for ResolvedJobArtifact { flowey_var, name } in artifacts_used {
            let var_db_inject_cmd = bootstrap_bash_var_db_inject(flowey_var, true);
            match platform.kind() {
                FlowPlatformKind::Windows => {
                    writeln!(
                        flowey_bootstrap_bash,
                        r#"echo "{RUNNER_TEMP}\\used_artifacts\\{name}" | {var_db_inject_cmd}"#,
                    )?;
                }
                FlowPlatformKind::Unix => {
                    writeln!(
                        flowey_bootstrap_bash,
                        r#"echo "$AgentTempDirNormal/used_artifacts/{name}" | {var_db_inject_cmd}"#,
                    )?;
                }
            }
        }

        gh_steps.push({
            let mut map = serde_yaml::Mapping::new();
            map.insert("name".into(), "🌼🛫 Initialize job".into());
            map.insert(
                "run".into(),
                serde_yaml::Value::String(flowey_bootstrap_bash),
            );
            map.insert("shell".into(), "bash".into());
            map.into()
        });

        // now that we've done all the job-level bootstrapping, we can emit all
        // the actual steps the user cares about
        gh_steps.extend(steps);

        // ..and once that's done, the last order of business is to emit some
        // GitHub steps to publish the various artifacts created by this job
        for ResolvedJobArtifact {
            flowey_var: _,
            name,
        } in artifacts_published
        {
            gh_steps.push({
                let map: serde_yaml::Mapping = serde_yaml::from_str(&format!(
                    r#"
                        name: 🌼📦 Publish {name}
                        uses: actions/upload-artifact@v4
                        with:
                            name: {name}
                            path: {RUNNER_TEMP}/publish_artifacts/{name}/
                            include-hidden-files: true
                    "#
                ))
                .unwrap();
                map.into()
            });
        }

        // also, if this job also bootstrapped flowey that other nodes depend
        // on, make sure to publish it!
        if let FloweySource::Bootstrap(artifact, true) = flowey_source {
            // don't leak the bootstrap job's runtime var db
            gh_steps.push({
                let mut map = serde_yaml::Mapping::new();
                map.insert("name".into(), "🌼🧼 Redact bootstrap var db".into());
                map.insert(
                    "run".into(),
                    serde_yaml::Value::String(format!(
                        "rm $AgentTempDirNormal/{flowey_path}/job{}.json",
                        job_idx.index()
                    )),
                );
                map.insert("shell".into(), "bash".into());
                map.into()
            });

            gh_steps.push({
                let map: serde_yaml::Mapping = serde_yaml::from_str(&format!(
                    r#"
                    name: 🌼🥾 Publish bootstrapped flowey
                    uses: actions/upload-artifact@v4
                    with:
                        name: {artifact}
                        path: {RUNNER_TEMP}/{flowey_path}
                "#
                ))
                .unwrap();
                map.into()
            });
        }

        let runner_kind_to_yaml = |runner: &GhRunner| match runner {
            GhRunner::GhHosted(s) => github_yaml_defs::Runner::GhHosted(match s {
                GhRunnerOsLabel::UbuntuLatest => github_yaml_defs::RunnerOsLabel::UbuntuLatest,
                GhRunnerOsLabel::Ubuntu2404 => github_yaml_defs::RunnerOsLabel::Ubuntu2404,
                GhRunnerOsLabel::Ubuntu2204 => github_yaml_defs::RunnerOsLabel::Ubuntu2204,
                GhRunnerOsLabel::WindowsLatest => github_yaml_defs::RunnerOsLabel::WindowsLatest,
                GhRunnerOsLabel::Windows2025 => github_yaml_defs::RunnerOsLabel::Windows2025,
                GhRunnerOsLabel::Windows2022 => github_yaml_defs::RunnerOsLabel::Windows2022,
                GhRunnerOsLabel::Ubuntu2404Arm => github_yaml_defs::RunnerOsLabel::Ubuntu2404Arm,
                GhRunnerOsLabel::Ubuntu2204Arm => github_yaml_defs::RunnerOsLabel::Ubuntu2204Arm,
                GhRunnerOsLabel::Windows11Arm => github_yaml_defs::RunnerOsLabel::Windows11Arm,
                GhRunnerOsLabel::Custom(s) => github_yaml_defs::RunnerOsLabel::Custom(s.into()),
            }),
            GhRunner::SelfHosted(v) => github_yaml_defs::Runner::SelfHosted(v.clone()),
            GhRunner::RunnerGroup { group, labels } => github_yaml_defs::Runner::Group {
                group: group.into(),
                labels: labels.clone(),
            },
        };

        let perm_val_to_yaml = |permission_value: &GhPermissionValue| match permission_value {
            GhPermissionValue::Read => github_yaml_defs::PermissionValue::Read,
            GhPermissionValue::Write => github_yaml_defs::PermissionValue::Write,
            GhPermissionValue::None => github_yaml_defs::PermissionValue::None,
        };

        let perm_kind_to_yaml = |permission: &GhPermission| match permission {
            GhPermission::Actions => github_yaml_defs::Permissions::Actions,
            GhPermission::Attestations => github_yaml_defs::Permissions::Attestations,
            GhPermission::Checks => github_yaml_defs::Permissions::Checks,
            GhPermission::Contents => github_yaml_defs::Permissions::Contents,
            GhPermission::Deployments => github_yaml_defs::Permissions::Deployments,
            GhPermission::Discussions => github_yaml_defs::Permissions::Discussions,
            GhPermission::IdToken => github_yaml_defs::Permissions::IdToken,
            GhPermission::Issues => github_yaml_defs::Permissions::Issues,
            GhPermission::Packages => github_yaml_defs::Permissions::Packages,
            GhPermission::Pages => github_yaml_defs::Permissions::Pages,
            GhPermission::PullRequests => github_yaml_defs::Permissions::PullRequests,
            GhPermission::RepositoryProjects => github_yaml_defs::Permissions::RepositoryProjects,
            GhPermission::SecurityEvents => github_yaml_defs::Permissions::SecurityEvents,
            GhPermission::Statuses => github_yaml_defs::Permissions::Statuses,
        };

        let mut job_permissions = BTreeMap::new();
        for permission_map in gh_permissions.values() {
            for (permission, value) in permission_map {
                if let Some(old_value) = job_permissions.insert(permission.clone(), value.clone()) {
                    if old_value != *value {
                        anyhow::bail!(
                            "permission {:?} was to conflicting values in job {:?}: {:?} and {:?}",
                            permission,
                            label,
                            old_value,
                            value
                        )
                    }
                };
            }
        }

        github_jobs.insert(
            format!("job{}", job_idx.index()),
            github_yaml_defs::Job {
                name: label.clone(),
                timeout_minutes,
                runs_on: gh_pool.clone().map(|runner| runner_kind_to_yaml(&runner)),
                permissions: job_permissions
                    .iter()
                    .map(|k| (perm_kind_to_yaml(k.0), perm_val_to_yaml(k.1)))
                    .collect(),
                needs: {
                    graph
                        .edges_directed(job_idx, petgraph::Direction::Incoming)
                        .map(|e| {
                            use petgraph::prelude::*;
                            format!("job{}", e.source().index())
                        })
                        .collect()
                },
                r#if: gh_override_if
                    .clone()
                    .or_else(|| Some("github.event.pull_request.draft == false".to_string())),
                env: gh_global_env.clone(),
                steps: gh_steps,
            },
        );
    }

    let mut concurrency = None;
    let pipeline_trigger = github_yaml_defs::Triggers {
        workflow_call: None,
        workflow_dispatch: Some(github_yaml_defs::WorkflowDispatch {
            inputs: github_yaml_defs::Inputs {
                inputs: parameters
                    .into_iter()
                    .map(|param| {
                        (
                            param.name().to_string(),
                            match param {
                                flowey_core::pipeline::internal::Parameter::Bool {
                                    name: _,
                                    description,
                                    kind: _,
                                    default,
                                } => github_yaml_defs::Input {
                                    description: Some(description.clone()),
                                    default: default.map(github_yaml_defs::Default::Boolean),
                                    required: default.is_none(),
                                    ty: github_yaml_defs::InputType::Boolean,
                                },
                                flowey_core::pipeline::internal::Parameter::String {
                                    name: _,
                                    description,
                                    kind: _,
                                    default,
                                    possible_values: _,
                                } => github_yaml_defs::Input {
                                    description: Some(description.clone()),
                                    default: default
                                        .as_ref()
                                        .map(|s| github_yaml_defs::Default::String(s.clone())),
                                    required: default.is_none(),
                                    ty: github_yaml_defs::InputType::String,
                                },
                                flowey_core::pipeline::internal::Parameter::Num {
                                    name: _,
                                    description,
                                    kind: _,
                                    default,
                                    possible_values: _,
                                } => github_yaml_defs::Input {
                                    description: Some(description.clone()),
                                    default: default.map(github_yaml_defs::Default::Number),
                                    required: default.is_none(),
                                    ty: github_yaml_defs::InputType::Number,
                                },
                            },
                        )
                    })
                    .collect::<BTreeMap<String, github_yaml_defs::Input>>(),
            },
        }),
        pull_request: match gh_pr_triggers {
            Some(gh_pr_triggers) => {
                if gh_pr_triggers.auto_cancel {
                    concurrency = Some(github_yaml_defs::Concurrency {
                        // only cancel in-progress jobs for the same workflow and branch
                        group: Some("${{ github.workflow }}-${{ github.ref }}".to_string()),
                        cancel_in_progress: Some(true),
                    })
                };
                Some(github_yaml_defs::PrTrigger {
                    branches: gh_pr_triggers.branches.clone(),
                    branches_ignore: gh_pr_triggers.exclude_branches.clone(),
                    types: gh_pr_triggers.types.clone(),
                })
            }
            None => None,
        },
        push: match gh_ci_triggers {
            Some(gh_ci_triggers) => Some(github_yaml_defs::CiTrigger {
                branches: gh_ci_triggers.branches,
                branches_ignore: gh_ci_triggers.exclude_branches,
                tags: gh_ci_triggers.tags,
                tags_ignore: gh_ci_triggers.exclude_tags,
            }),
            None => None,
        },
        schedule: gh_schedule_triggers
            .iter()
            .map(|s| github_yaml_defs::Cron {
                cron: s.cron.clone(),
            })
            .collect(),
    };

    let github_pipeline = github_yaml_defs::Pipeline {
        name: gh_name,
        on: Some(pipeline_trigger),
        concurrency,
        jobs: Some(github_yaml_defs::Jobs { jobs: github_jobs }),
        inputs: None,
    };

    match check {
        CheckMode::Check(_) | CheckMode::Runtime(_) => check_generated_yaml_and_json(
            &github_pipeline,
            &pipeline_static_db,
            check,
            repo_root,
            pipeline_file,
            None,
        ),
        CheckMode::None => write_generated_yaml_and_json(
            &github_pipeline,
            &pipeline_static_db,
            repo_root,
            pipeline_file,
            None,
        ),
    }
}

/// Resolve a flow as a sequence of GitHub YAML steps.
///
/// These steps can then be marshalled into a well-formed GitHub pipeline yaml
/// using a separate GitHub pipeline yaml builder
// pub(crate) so that internal debug CLI tooling can use it
fn resolve_flow_as_github_yaml_steps(
    seed_nodes: BTreeMap<NodeHandle, (bool, Vec<Box<[u8]>>)>,
    resolved_patches: flowey_core::patch::ResolvedPatches,
    external_read_vars: BTreeSet<String>,
    platform: FlowPlatform,
    arch: FlowArch,
    job_idx: usize,
    flowey_bin: &str,
    gh_permissions: &BTreeMap<NodeHandle, BTreeMap<GhPermission, GhPermissionValue>>,
) -> anyhow::Result<(
    Vec<serde_yaml::Value>,
    BTreeMap<String, Vec<crate::cli::exec_snippet::SerializedRequest>>,
)> {
    let mut output_steps = Vec::new();

    let (mut output_graph, request_db, err_unreachable_nodes) =
        crate::flow_resolver::stage1_dag::stage1_dag(
            FlowBackend::Github,
            platform,
            arch,
            resolved_patches,
            seed_nodes,
            external_read_vars,
            // TODO: support GitHub agents with persistent storage
            None,
        )?;

    if err_unreachable_nodes.is_some() {
        anyhow::bail!("detected unreachable nodes")
    }

    let mut bash_commands = BashCommands::new_github();

    let output_order = petgraph::algo::toposort(&output_graph, None)
        .expect("runtime variables cannot introduce a DAG cycle");

    let var_db = VarDbRequestBuilder::new(flowey_bin, job_idx);

    for node_idx in output_order.into_iter().rev() {
        let OutputGraphEntry { node_handle, step } = output_graph[node_idx].1.take().unwrap();

        let node_modpath = node_handle.modpath();

        match step {
            Step::Anchor { .. } => {}
            Step::Rust {
                idx,
                label,
                can_merge,
                code: _,
            } => {
                output_steps.extend(bash_commands.push(
                    Some(label),
                    can_merge,
                    crate::cli::exec_snippet::construct_exec_snippet_cli(
                        flowey_bin,
                        node_modpath,
                        idx,
                        job_idx,
                    ),
                ));
            }
            Step::AdoYaml { label, .. } => {
                anyhow::bail!("ADO YAML not supported in GitHub. In step '{}'", label)
            }
            Step::GitHubYaml {
                gh_to_rust,
                rust_to_gh,
                label,
                step_id,
                uses,
                with,
                condvar,
                permissions,
            } => {
                for permission in permissions {
                    if let Some(permission_map) = gh_permissions.get(&node_handle) {
                        if let Some(permission_value) = permission_map.get(&permission.0) {
                            if *permission_value != permission.1 {
                                anyhow::bail!(
                                    "permission mismatch for {:?}: expected {:?}, got {:?}",
                                    permission.0,
                                    permission.1,
                                    permission_value
                                )
                            }
                        }
                    } else {
                        anyhow::bail!(
                            "permission missing for {:?}: expected {:?}",
                            permission.0,
                            permission.1
                        )
                    }
                }

                for gh_var_state in rust_to_gh {
                    let set_gh_env_var = var_db
                        .write_to_gh_env(&gh_var_state.backing_var, &gh_var_state.raw_name)
                        .raw_string(!gh_var_state.is_object)
                        .condvar(condvar.as_deref());

                    bash_commands.push_minor(format!("{set_gh_env_var}\n"));
                }

                if !uses.is_empty() {
                    if let Some(condvar) = &condvar {
                        // guaranteed to be a bare bool `true`/`false`, hence
                        // is_raw_string = false
                        let set_condvar = var_db.write_to_gh_env(condvar, "FLOWEY_CONDITION");
                        bash_commands.push_minor(format!("{set_condvar}\n"));
                    }

                    let mut map = serde_yaml::Mapping::new();
                    map.insert("id".into(), serde_yaml::Value::String(step_id.clone()));
                    map.insert("uses".into(), serde_yaml::Value::String(uses));
                    if !with.is_empty() {
                        let mut with_map = serde_yaml::Mapping::new();
                        for (k, v) in with {
                            with_map.insert(k.into(), v.into());
                        }
                        map.insert("with".into(), with_map.into());
                    }
                    map.insert("name".into(), label.into());
                    if condvar.is_some() {
                        map.insert("if".into(), "${{ fromJSON(env.FLOWEY_CONDITION) }}".into());
                    }

                    let step: serde_yaml::Value = map.into();
                    output_steps.extend(bash_commands.flush());
                    output_steps.push(step);
                }

                for gh_var_state in gh_to_rust {
                    let value = if gh_var_state.is_object {
                        format!(r#"${{{{ toJSON({}) }}}}"#, gh_var_state.raw_name)
                    } else {
                        format!(r#"${{{{ {} }}}}"#, gh_var_state.raw_name)
                    };

                    let write_var = var_db
                        .update_from_stdin(&gh_var_state.backing_var, gh_var_state.is_secret)
                        .raw_string(!gh_var_state.is_object)
                        .condvar(condvar.as_deref())
                        .env_source(Some(&gh_var_state.raw_name));

                    let cmd = format!("{write_var} <<EOF\n{value}\nEOF",);
                    bash_commands.push_minor(cmd);
                }
            }
        }
    }

    output_steps.extend(bash_commands.flush());

    let request_db = request_db
        .into_iter()
        .map(|(node_handle, reqs)| {
            (
                node_handle.modpath().to_owned(),
                reqs.into_iter()
                    .map(crate::cli::exec_snippet::SerializedRequest)
                    .collect(),
            )
        })
        .collect();

    Ok((output_steps, request_db))
}
