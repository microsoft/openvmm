// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::common_yaml::BashCommands;
use super::common_yaml::FloweySource;
use super::common_yaml::check_generated_yaml_and_json;
use super::common_yaml::job_flowey_bootstrap_source;
use super::common_yaml::write_generated_yaml_and_json;
use super::generic::ResolvedJobArtifact;
use super::generic::ResolvedJobUseParameter;
use crate::cli::exec_snippet::FloweyPipelineStaticDb;
use crate::cli::exec_snippet::VAR_DB_SEEDVAR_FLOWEY_WORKING_DIR;
use crate::cli::pipeline::CheckMode;
use crate::cli::var_db::VarDbRequestBuilder;
use crate::flow_resolver::stage1_dag::OutputGraphEntry;
use crate::flow_resolver::stage1_dag::Step;
use crate::pipeline_resolver::generic::ResolvedPipeline;
use crate::pipeline_resolver::generic::ResolvedPipelineJob;
use anyhow::Context;
use flowey_core::node::FlowArch;
use flowey_core::node::FlowBackend;
use flowey_core::node::FlowPlatform;
use flowey_core::node::FlowPlatformKind;
use flowey_core::node::NodeHandle;
use flowey_core::pipeline::internal::AdoPool;
use flowey_core::pipeline::internal::InternalAdoResourcesRepository;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::Path;

/// We use $(Build.StagingDirectory)/.flowey-internal instead
/// of $(Agent.TempDirectory) to (hopefully) guarantee that this folder
/// resides on the same mount-point as the repos being cloned.
///
/// violating this property would result in calls to `fs::rename` in
/// downstream flowey nodes to fail.
const FLOWEY_TEMP_DIR: &str = "$(Build.StagingDirectory)/.flowey-internal";

/// Emit a pipeline as a single self-contained ADO yaml file
pub fn ado_yaml(
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
        parameters,
        ado_name,
        ado_schedule_triggers,
        ado_ci_triggers,
        ado_pr_triggers,
        ado_bootstrap_template,
        ado_resources_repository,
        ado_post_process_yaml_cb,
        ado_variables,
        ref ado_job_id_overrides,
        gh_name: _,
        gh_schedule_triggers: _,
        gh_ci_triggers: _,
        gh_pr_triggers: _,
        gh_bootstrap_template: _,
    } = pipeline;

    let mut job_flowey_source: BTreeMap<petgraph::prelude::NodeIndex, FloweySource> =
        job_flowey_bootstrap_source(&graph, &order);

    let mut pipeline_static_db = FloweyPipelineStaticDb {
        flow_backend: crate::cli::FlowBackendCli::Ado,
        var_db_backend_kind: crate::cli::exec_snippet::VarDbBackendKind::Json,
        job_reqs: BTreeMap::new(),
    };

    let mut ado_jobs = Vec::new();

    for job_idx in order {
        let ResolvedPipelineJob {
            ref root_nodes,
            ref patches,
            ref label,
            platform,
            arch,
            cond_param_idx,
            ref ado_pool,
            timeout_minutes,
            gh_override_if: _,
            gh_global_env: _,
            gh_pool: _,
            gh_permissions: _,
            ref external_read_vars,
            ref parameters_used,
            ref artifacts_used,
            ref artifacts_published,
            ref ado_variables,
        } = graph[job_idx];

        let flowey_source = job_flowey_source.remove(&job_idx).unwrap();

        let (steps, req_db) = resolve_flow_as_ado_yaml_steps(
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
        )
        .context(format!("in job '{label}'"))?;

        {
            let existing = pipeline_static_db.job_reqs.insert(job_idx.index(), req_db);
            assert!(existing.is_none())
        }

        let mut ado_steps = Vec::new();

        if let FloweySource::Bootstrap(artifact, publish) = &flowey_source {
            // actual artifact publish happens at the end of the job
            let _ = (artifact, publish);

            if ado_bootstrap_template.is_empty() {
                anyhow::bail!(
                    "Did not specify flowey bootstrap template. Please provide one using `Pipeline::ado_set_flowey_bootstrap_template`"
                )
            }

            let ado_bootstrap_template = ado_bootstrap_template
                .replace("{{FLOWEY_BIN_EXTENSION}}", platform.exe_suffix())
                .replace("{{FLOWEY_CRATE}}", flowey_crate)
                .replace(
                    "{{FLOWEY_PIPELINE_PATH}}",
                    &pipeline_file.with_extension("").display().to_string(),
                )
                .replace(
                    "{{FLOWEY_TARGET}}",
                    match platform {
                        FlowPlatform::Windows => "x86_64-pc-windows-msvc",
                        FlowPlatform::Linux(_) => "x86_64-unknown-linux-gnu",
                        platform => anyhow::bail!("unsupported ADO platform {platform:?}"),
                    },
                )
                .replace(
                    "{{FLOWEY_OUTDIR}}",
                    "$(FLOWEY_TEMP_DIR)/bootstrapped-flowey",
                );

            let bootstrap_steps: serde_yaml::Sequence =
                serde_yaml::from_str(&ado_bootstrap_template)
                    .context("malformed flowey bootstrap template")?;

            ado_steps.extend(bootstrap_steps);
        }

        // the first few steps in any job are some "artisan" code, which
        // downloads the previously bootstrapped flowey artifact and set up
        // various vars that flowey will then rely on throughout the rest
        // of the job

        // download previously bootstrapped flowey
        if let FloweySource::Consume(artifact) = &flowey_source {
            ado_steps.push({
                let map: serde_yaml::Mapping = serde_yaml::from_str(&format!(
                    r#"
                        task: DownloadPipelineArtifact@2
                        displayName: '🌼🥾 Download bootstrapped flowey'
                        inputs:
                          artifact: {artifact}
                          path: $(FLOWEY_TEMP_DIR)/bootstrapped-flowey
                    "#
                ))
                .unwrap();
                map.into()
            });
        }

        // also download any artifacts that'll be used
        for ResolvedJobArtifact {
            flowey_var: _,
            name,
        } in artifacts_used
        {
            ado_steps.push({
                let map: serde_yaml::Mapping = serde_yaml::from_str(&format!(
                    r#"
                        task: DownloadPipelineArtifact@2
                        displayName: '🌼📦 Download {name}'
                        inputs:
                          artifact: {name}
                          path: $(FLOWEY_TEMP_DIR)/used_artifacts/{name}
                    "#
                ))
                .unwrap();
                map.into()
            });
        }

        let flowey_bin = platform.binary("flowey");
        let flowey_executable_bash = format!(
            r###"
set -e
AgentTempDirNormal="$(FLOWEY_TEMP_DIR)"
AgentTempDirNormal=$(echo "$AgentTempDirNormal" | sed -e 's|\\|\/|g' -e 's|^\([A-Za-z]\)\:/\(.*\)|/\L\1\E/\2|')
echo "##vso[task.setvariable variable=AgentTempDirNormal;]$AgentTempDirNormal"

chmod +x $AgentTempDirNormal/bootstrapped-flowey/{flowey_bin}
FLOWEY_BIN="$AgentTempDirNormal/bootstrapped-flowey/{flowey_bin}"
echo "##vso[task.setvariable variable=FLOWEY_BIN;]$FLOWEY_BIN"
"###
        ).trim_start().to_string();

        ado_steps.push({
            let mut map = serde_yaml::Mapping::new();
            map.insert(
                "bash".into(),
                serde_yaml::Value::String(flowey_executable_bash),
            );
            map.insert("displayName".into(), "Set flowey path".into());
            map.into()
        });

        let mut flowey_bootstrap_bash = String::new();

        let var_db = VarDbRequestBuilder::new("$FLOWEY_BIN", job_idx.index());

        let bootstrap_bash_var_db_inject = |var, is_raw_string| {
            var_db
                .update_from_stdin(var, false)
                .raw_string(is_raw_string)
                .to_string()
        };

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
set -e
echo '"{runtime_debug_level}"' | {var_db_insert_runtime_debug_level}
echo "$(FLOWEY_TEMP_DIR)/work" | {var_db_insert_working_dir}
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
            let is_bool = matches!(
                parameters[*pipeline_param_idx],
                flowey_core::pipeline::internal::Parameter::Bool { .. }
            );

            let name = parameters[*pipeline_param_idx].name();

            // ADO resolves bools as `True` and `False`, _sigh_
            let with_lowercase = if is_bool {
                r#" | tr '[:upper:]' '[:lower:]'"#
            } else {
                ""
            };

            let var_db_inject_cmd = bootstrap_bash_var_db_inject(flowey_var, is_string);

            let cmd = format!(
                r#"
cat <<'EOF'{with_lowercase} | {var_db_inject_cmd}
${{{{ parameters.{name} }}}}
EOF
"#
            )
            .trim()
            .to_string();
            writeln!(flowey_bootstrap_bash, "{}", cmd)?;
        }

        // next, emit ado steps to create dirs for artifacts which will be
        // published
        for ResolvedJobArtifact { flowey_var, name } in artifacts_published {
            writeln!(
                flowey_bootstrap_bash,
                r#"mkdir -p "$(AgentTempDirNormal)/publish_artifacts/{name}""#
            )?;
            let var_db_inject_cmd = bootstrap_bash_var_db_inject(flowey_var, true);
            writeln!(
                flowey_bootstrap_bash,
                r#"echo "$(FLOWEY_TEMP_DIR)/publish_artifacts/{name}" | {var_db_inject_cmd}"#,
            )?;
        }

        // lastly, emit ado steps that report the dirs for any artifacts which
        // are used by this job
        for ResolvedJobArtifact { flowey_var, name } in artifacts_used {
            // do NOT use ADO macro syntax $(...), since this is in the same
            // bootstrap block as where those ADO vars get defined, meaning it's
            // not available yet!
            let var_db_inject_cmd = bootstrap_bash_var_db_inject(flowey_var, true);
            writeln!(
                flowey_bootstrap_bash,
                r#"echo "$(FLOWEY_TEMP_DIR)/used_artifacts/{name}" | {var_db_inject_cmd}"#,
            )?;
        }

        // if this was a bootstrap job, also take a moment to run a "self check"
        // to make sure that the current checked-in template matches the one it
        // expected
        if let FloweySource::Bootstrap(..) = &flowey_source {
            let mut current_invocation = std::env::args().collect::<Vec<_>>();

            current_invocation[0] = "$(FLOWEY_BIN)".into();

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
                        r#"$ESCAPED_AGENT_TEMPDIR\\bootstrapped-flowey\\pipeline.yaml"#
                    }
                    FlowPlatformKind::Unix => {
                        r#"$ESCAPED_AGENT_TEMPDIR/bootstrapped-flowey/pipeline.yaml"#
                    }
                };

                current_invocation.insert(i, current_yaml.into());
                current_invocation.insert(i, "--runtime".into());
            }

            // Need to use an escaped version of the "true" windows/linux path
            // here, or else the --check will fail.
            let cmd = format!(
                r###"
ESCAPED_AGENT_TEMPDIR=$(
cat <<'EOF' | sed 's/\\/\\\\/g'
$(FLOWEY_TEMP_DIR)
EOF
)
{}
"###,
                current_invocation.join(" ")
            );

            ado_steps.push({
                let mut map = serde_yaml::Mapping::new();
                map.insert(
                    "bash".into(),
                    serde_yaml::Value::String(cmd.trim().to_string()),
                );
                map.insert("displayName".into(), "🌼🔎 Self-check YAML".into());
                map.into()
            })
        }

        ado_steps.push({
            let mut map = serde_yaml::Mapping::new();
            map.insert(
                "bash".into(),
                serde_yaml::Value::String(flowey_bootstrap_bash),
            );
            map.insert("displayName".into(), "🌼🛫 Initialize job".into());
            map.into()
        });

        // now that we've done all the job-level bootstrapping, we can emit all
        // the actual steps the user cares about
        ado_steps.extend(steps);

        // ..and once that's done, the last order of business is to emit some
        // ado steps to publish the various artifacts created by this job
        for ResolvedJobArtifact {
            flowey_var: _,
            name,
        } in artifacts_published
        {
            ado_steps.push({
                let map: serde_yaml::Mapping = serde_yaml::from_str(&format!(
                    r#"
                        publish: $(FLOWEY_TEMP_DIR)/publish_artifacts/{name}
                        displayName: '🌼📦 Publish {name}'
                        artifact: {name}
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
            ado_steps.push({
                let mut map = serde_yaml::Mapping::new();
                map.insert(
                    "bash".into(),
                    serde_yaml::Value::String(format!(
                        "rm $(AgentTempDirNormal)/bootstrapped-flowey/job{}.json",
                        job_idx.index()
                    )),
                );
                map.insert("displayName".into(), "🌼🧼 Redact bootstrap var db".into());
                map.into()
            });

            ado_steps.push({
                let map: serde_yaml::Mapping = serde_yaml::from_str(&format!(
                    r#"
                    publish: $(FLOWEY_TEMP_DIR)/bootstrapped-flowey
                    displayName: '🌼🥾 Publish bootstrapped flowey'
                    artifact: {artifact}
                "#
                ))
                .unwrap();
                map.into()
            });
        }

        // ADO has this "helpful" default behavior where if you don't explicitly
        // include a checkout step, it'll just auto-checkout the current repo.
        //
        // Work around this nonsense by doing a pre-pass over the emitted
        // steps to enumerate how many steps start with `- checkout:`, and
        // if the number is zero, emit an explicit `- checkout: none`.
        {
            let mut found = false;
            for val in &ado_steps {
                if let Some((key, _val)) = val.as_mapping().unwrap().iter().next() {
                    let Some(key) = key.as_str() else { continue };
                    if key == "checkout" {
                        found = true;
                        break;
                    }
                }
            }
            if !found {
                ado_steps.insert(0, {
                    let map: serde_yaml::Mapping = serde_yaml::from_str("checkout: none").unwrap();
                    map.into()
                });
            }
        }

        // Convert the pool information to the structured format
        let AdoPool {
            name: pool_name,
            demands,
        } = ado_pool
            .clone()
            .context(format!("must specify ADO pool for job '{label}'"))?;
        let pool = if demands.is_empty() {
            schema_ado_yaml::Pool::Pool(pool_name)
        } else {
            schema_ado_yaml::Pool::PoolWithMetadata(
                [
                    ("name".into(), pool_name.into()),
                    ("demands".into(), demands.into()),
                ]
                .into(),
            )
        };

        let get_job_id = |id: usize| {
            ado_job_id_overrides
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("job{}", id.clone()))
        };
        ado_jobs.push(schema_ado_yaml::Job {
            job: get_job_id(job_idx.index()),
            display_name: label.clone(),
            pool,
            timeout_in_minutes: timeout_minutes,
            depends_on: {
                graph
                    .edges_directed(job_idx, petgraph::Direction::Incoming)
                    .map(|e| {
                        use petgraph::prelude::*;
                        get_job_id(e.source().index())
                    })
                    .collect()
            },
            variables: {
                let mut ado_variables: Vec<schema_ado_yaml::Variable> = ado_variables
                    .clone()
                    .into_iter()
                    .map(|(name, value)| schema_ado_yaml::Variable { name, value })
                    .collect();

                ado_variables.push(schema_ado_yaml::Variable {
                    name: "FLOWEY_TEMP_DIR".into(),
                    value: FLOWEY_TEMP_DIR.into(),
                });

                Some(ado_variables)
            },
            steps: ado_steps,
            condition: Some(if let Some(cond_param_idx) = cond_param_idx {
                format!(
                    "and(eq('${{{{ parameters.{} }}}}', 'true'), succeeded(), not(canceled()))",
                    parameters[cond_param_idx].name()
                )
            } else {
                "and(succeeded(), not(canceled()))".into()
            }),
        })
    }

    let ado_pipeline = schema_ado_yaml::Pipeline {
        name: ado_name,
        trigger: Some(match ado_ci_triggers {
            None => schema_ado_yaml::CiTrigger::None(()),
            Some(t) => {
                let flowey_core::pipeline::AdoCiTriggers {
                    branches,
                    exclude_branches,
                    tags,
                    exclude_tags,
                    batch,
                } = t;

                if branches.is_empty() && tags.is_empty() {
                    anyhow::bail!("branches and tags cannot both be empty")
                }

                schema_ado_yaml::CiTrigger::Some {
                    batch,
                    branches: if branches.is_empty() {
                        if !exclude_branches.is_empty() {
                            anyhow::bail!("empty branch trigger with non-empty exclude")
                        }

                        None
                    } else {
                        Some(schema_ado_yaml::TriggerBranches {
                            include: branches,
                            exclude: if exclude_branches.is_empty() {
                                None
                            } else {
                                Some(exclude_branches)
                            },
                        })
                    },
                    tags: if tags.is_empty() {
                        if !exclude_tags.is_empty() {
                            anyhow::bail!("empty tags trigger with non-empty exclude")
                        }

                        None
                    } else {
                        Some(schema_ado_yaml::TriggerTags {
                            include: tags,
                            exclude: if exclude_tags.is_empty() {
                                None
                            } else {
                                Some(exclude_tags)
                            },
                        })
                    },
                }
            }
        }),
        pr: Some(match ado_pr_triggers {
            None => schema_ado_yaml::PrTrigger::None(()),
            Some(t) => {
                let flowey_core::pipeline::AdoPrTriggers {
                    branches,
                    exclude_branches,
                    run_on_draft,
                    auto_cancel,
                } = t;

                schema_ado_yaml::PrTrigger::Some {
                    auto_cancel,
                    drafts: run_on_draft,
                    branches: schema_ado_yaml::TriggerBranches {
                        include: branches,
                        exclude: if exclude_branches.is_empty() {
                            None
                        } else {
                            Some(exclude_branches)
                        },
                    },
                }
            }
        }),
        schedules: if ado_schedule_triggers.is_empty() {
            None
        } else {
            Some(
                ado_schedule_triggers
                    .into_iter()
                    .map(|t| {
                        let flowey_core::pipeline::AdoScheduleTriggers {
                            display_name,
                            branches,
                            exclude_branches,
                            cron,
                        } = t;

                        schema_ado_yaml::Schedule {
                            cron,
                            display_name,
                            branches: schema_ado_yaml::TriggerBranches {
                                include: branches,
                                exclude: if exclude_branches.is_empty() {
                                    None
                                } else {
                                    Some(exclude_branches)
                                },
                            },
                            batch: false,
                        }
                    })
                    .collect(),
            )
        },
        variables: if !ado_variables.is_empty() {
            Some(
                ado_variables
                    .into_iter()
                    .map(|(name, value)| schema_ado_yaml::Variable { name, value })
                    .collect(),
            )
        } else {
            None
        },
        stages: None,
        jobs: Some(ado_jobs),
        parameters: if !parameters.is_empty() {
            Some(
                parameters
                    .clone()
                    .into_iter()
                    .map(|param| match param {
                        flowey_core::pipeline::internal::Parameter::Bool {
                            name,
                            description,
                            kind: _,
                            default,
                        } => schema_ado_yaml::Parameter {
                            name,
                            display_name: description,
                            ty: schema_ado_yaml::ParameterType::Boolean { default },
                        },
                        flowey_core::pipeline::internal::Parameter::String {
                            name,
                            description,
                            kind: _,
                            default,
                            possible_values,
                        } => schema_ado_yaml::Parameter {
                            name,
                            display_name: description,
                            ty: schema_ado_yaml::ParameterType::String {
                                default,
                                values: possible_values,
                            },
                        },
                        flowey_core::pipeline::internal::Parameter::Num {
                            name,
                            description,
                            kind: _,
                            default,
                            possible_values,
                        } => schema_ado_yaml::Parameter {
                            name,
                            display_name: description,
                            ty: schema_ado_yaml::ParameterType::Number {
                                default,
                                values: possible_values,
                            },
                        },
                    })
                    .collect(),
            )
        } else {
            None
        },
        resources: {
            if ado_resources_repository.is_empty() {
                None
            } else {
                Some(schema_ado_yaml::Resources {
                    repositories: ado_resources_repository
                        .into_iter()
                        .map(
                            |InternalAdoResourcesRepository {
                                 repo_id,
                                 repo_type,
                                 name,
                                 git_ref,
                                 endpoint,
                             }| {
                                use flowey_core::pipeline::AdoResourcesRepositoryRef;
                                use flowey_core::pipeline::AdoResourcesRepositoryType;

                                schema_ado_yaml::ResourcesRepository {
                                    repository: repo_id,
                                    endpoint,
                                    name,
                                    r#ref: match git_ref {
                                        AdoResourcesRepositoryRef::Fixed(s) => s,
                                        AdoResourcesRepositoryRef::Parameter(idx) => {
                                            let name = parameters[idx].name();
                                            format!("${{{{ parameters.{name} }}}}")
                                        }
                                    },
                                    r#type: match repo_type {
                                        AdoResourcesRepositoryType::AzureReposGit => {
                                            schema_ado_yaml::ResourcesRepositoryType::Git
                                        }
                                        AdoResourcesRepositoryType::GitHub => {
                                            schema_ado_yaml::ResourcesRepositoryType::GitHub
                                        }
                                    },
                                }
                            },
                        )
                        .collect::<Vec<_>>(),
                })
            }
        },
        extends: None,
    };

    match check {
        CheckMode::Check(_) | CheckMode::Runtime(_) => check_generated_yaml_and_json(
            &ado_pipeline,
            &pipeline_static_db,
            check,
            repo_root,
            pipeline_file,
            ado_post_process_yaml_cb,
        ),
        CheckMode::None => write_generated_yaml_and_json(
            &ado_pipeline,
            &pipeline_static_db,
            repo_root,
            pipeline_file,
            ado_post_process_yaml_cb,
        ),
    }
}

/// Resolve a flow as a sequence of ADO YAML steps.
///
/// These steps can then be marshalled into a well-formed ADO pipeline yaml
/// using a separate ADO pipeline yaml builder
// pub(crate) so that internal debug CLI tooling can use it
pub(crate) fn resolve_flow_as_ado_yaml_steps(
    seed_nodes: BTreeMap<NodeHandle, (bool, Vec<Box<[u8]>>)>,
    resolved_patches: flowey_core::patch::ResolvedPatches,
    external_read_vars: BTreeSet<String>,
    platform: FlowPlatform,
    arch: FlowArch,
    job_idx: usize,
) -> anyhow::Result<(
    Vec<serde_yaml::Value>,
    BTreeMap<String, Vec<crate::cli::exec_snippet::SerializedRequest>>,
)> {
    let mut output_steps = Vec::new();

    let (mut output_graph, request_db, err_unreachable_nodes) =
        crate::flow_resolver::stage1_dag::stage1_dag(
            FlowBackend::Ado,
            platform,
            arch,
            resolved_patches,
            seed_nodes,
            external_read_vars,
            // TODO: support ADO agents with persistent storage
            None,
        )?;

    if err_unreachable_nodes.is_some() {
        anyhow::bail!("detected unreachable nodes")
    }

    let output_order = petgraph::algo::toposort(&output_graph, None)
        .expect("runtime variables cannot introduce a DAG cycle");

    let var_db = VarDbRequestBuilder::new("$FLOWEY_BIN", job_idx);

    let mut bash_commands = BashCommands::new_ado();
    for idx in output_order.into_iter().rev() {
        let OutputGraphEntry { node_handle, step } = output_graph[idx].1.take().unwrap();

        let node_modpath = node_handle.modpath();

        match step {
            Step::Anchor { .. } => {}
            Step::Rust {
                idx,
                can_merge,
                label,
                code: _,
            } => {
                output_steps.extend(bash_commands.push(
                    Some(label),
                    can_merge,
                    crate::cli::exec_snippet::construct_exec_snippet_cli(
                        "$(FLOWEY_BIN)",
                        node_modpath,
                        idx,
                        job_idx,
                    ),
                ));
            }
            Step::AdoYaml {
                label,
                raw_yaml,
                ado_to_rust,
                rust_to_ado,
                condvar,
                code_idx,
                code,
            } => {
                for (rust_var, ado_var) in rust_to_ado {
                    // flowey considers all ADO vars to be typed as raw strings
                    let read_rust_var = var_db
                        .write_to_ado_env(&rust_var, &ado_var)
                        .raw_string(true)
                        .condvar(condvar.as_deref());

                    bash_commands.push_minor(format!("{read_rust_var}\n"));
                }

                if !raw_yaml.is_empty() {
                    if let Some(condvar) = &condvar {
                        // guaranteed to be a bare bool `true`/`false`, hence
                        // is_raw_string = false
                        let read_condvar = var_db.write_to_ado_env(condvar, "FLOWEY_CONDITION");

                        bash_commands.push_minor(format!("{read_condvar}\n"));
                    }

                    let raw_yaml = if code.lock().is_some() {
                        let inline_snippet = crate::cli::exec_snippet::construct_exec_snippet_cli(
                            "$(FLOWEY_BIN)",
                            node_modpath,
                            code_idx,
                            job_idx,
                        );
                        let post_process =
                            raw_yaml.replace("{{FLOWEY_INLINE_SCRIPT}}", &inline_snippet);
                        if raw_yaml == post_process {
                            return Err(anyhow::anyhow!("if using inlins-enippet, YAML must include {{{{FLOWEY_INLINE_SCRIPT}}}}").context(format!(
                                "invalid yaml in node {node_modpath}: {raw_yaml}"
                            )));
                        }
                        post_process
                    } else {
                        raw_yaml
                    };

                    let step: serde_yaml::Value = serde_yaml::from_str(&raw_yaml)
                        .context(format!("invalid yaml in node {node_modpath}: {raw_yaml}"))?;
                    let step = {
                        let mut step = step;
                        let seq = step
                            .as_sequence_mut()
                            .context("yaml snippet did not parse as a sequence")?;

                        if seq.len() != 1 {
                            anyhow::bail!("yaml snippet contained more than one sequence element")
                        }

                        let map = seq
                            .first_mut()
                            .unwrap()
                            .as_mapping_mut()
                            .context("yaml snippet did not parse as a map")?;
                        let existing = map.insert("displayName".into(), label.into());
                        if existing.is_some() {
                            anyhow::bail!("yaml snippet included `displayName`")
                        }
                        if condvar.is_some() {
                            let existing = map.insert(
                                "condition".into(),
                                "and(eq(variables['FLOWEY_CONDITION'], true), succeeded(), not(canceled()))".into(),
                            );
                            if existing.is_some() {
                                anyhow::bail!("yaml snippet included `condition`")
                            }
                        }

                        step
                    };
                    output_steps.extend(bash_commands.flush());
                    output_steps.push(step.as_sequence().unwrap().first().unwrap().clone());
                }

                for (ado_var, rust_var, is_secret) in ado_to_rust {
                    // flowey considers all ADO vars to be typed as raw strings
                    let write_rust_var = var_db
                        .update_from_stdin(&rust_var, is_secret)
                        .raw_string(true)
                        .condvar(condvar.as_deref())
                        .env_source(Some(&ado_var));

                    let cmd = format!(
                        r#"
{write_rust_var} <<'EOF'
$({ado_var})
EOF
"#
                    )
                    .trim()
                    .to_string();

                    bash_commands.push_minor(cmd);
                }
            }
            Step::GitHubYaml { label, .. } => anyhow::bail!("GitHub YAML not supported in ADO: {label}"),
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
