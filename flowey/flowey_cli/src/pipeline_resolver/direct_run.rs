// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::cli::exec_snippet::VAR_DB_SEEDVAR_FLOWEY_PERSISTENT_STORAGE_DIR;
use crate::flow_resolver::stage1_dag::OutputGraphEntry;
use crate::flow_resolver::stage1_dag::Step;
use crate::pipeline_resolver::generic::ResolvedJobArtifact;
use crate::pipeline_resolver::generic::ResolvedJobUseParameter;
use crate::pipeline_resolver::generic::ResolvedPipeline;
use crate::pipeline_resolver::generic::ResolvedPipelineJob;
use flowey_core::node::FlowArch;
use flowey_core::node::FlowBackend;
use flowey_core::node::FlowPlatform;
use flowey_core::node::NodeHandle;
use flowey_core::node::RuntimeVarDb;
use flowey_core::node::steps::rust::RustRuntimeServices;
use flowey_core::pipeline::HostExt;
use flowey_core::pipeline::PipelineBackendHint;
use flowey_core::pipeline::internal::Parameter;
use petgraph::prelude::NodeIndex;
use petgraph::visit::EdgeRef;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

struct ResolvedRunnableStep {
    node_handle: NodeHandle,
    label: String,
    code: Box<dyn for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()> + 'static>,
    idx: usize,
    can_merge: bool,
}

/// Directly run the pipeline using flowey
pub fn direct_run(
    pipeline: ResolvedPipeline,
    windows_as_wsl: bool,
    out_dir: PathBuf,
    persist_dir: PathBuf,
) -> anyhow::Result<()> {
    direct_run_do_work(pipeline, windows_as_wsl, out_dir.clone(), persist_dir)?;

    // cleanup
    if out_dir.join(".job_artifacts").exists() {
        fs_err::remove_dir_all(out_dir.join(".job_artifacts"))?;
    }
    if out_dir.join(".work").exists() {
        fs_err::remove_dir_all(out_dir.join(".work"))?;
    }

    Ok(())
}

fn direct_run_do_work(
    pipeline: ResolvedPipeline,
    windows_as_wsl: bool,
    out_dir: PathBuf,
    persist_dir: PathBuf,
) -> anyhow::Result<()> {
    fs_err::create_dir_all(&out_dir)?;
    let out_dir = std::path::absolute(out_dir)?;

    fs_err::create_dir_all(&persist_dir)?;
    let persist_dir = std::path::absolute(persist_dir)?;

    let ResolvedPipeline {
        graph,
        order,
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
        gh_name: _,
        gh_schedule_triggers: _,
        gh_ci_triggers: _,
        gh_pr_triggers: _,
        gh_bootstrap_template: _,
    } = pipeline;

    let mut skipped_jobs = BTreeSet::new();

    for idx in order {
        let ResolvedPipelineJob {
            ref root_nodes,
            ref patches,
            ref label,
            platform,
            arch,
            cond_param_idx,
            timeout_minutes: _,
            ado_pool: _,
            ado_variables: _,
            gh_override_if: _,
            gh_global_env: _,
            gh_pool: _,
            gh_permissions: _,
            ref external_read_vars,
            ref parameters_used,
            ref artifacts_used,
            ref artifacts_published,
        } = graph[idx];

        // orange color
        log::info!("\x1B[0;33m### job: {label} ###\x1B[0m");
        log::info!("");

        if graph
            .edges_directed(idx, petgraph::Direction::Incoming)
            .any(|e| skipped_jobs.contains(&NodeIndex::from(e.source().index() as u32)))
        {
            log::error!("job depends on job that was skipped. skipping job...");
            log::info!("");
            skipped_jobs.insert(idx);
            continue;
        }

        let flow_arch = FlowArch::host(PipelineBackendHint::Local);
        match (arch, flow_arch) {
            (FlowArch::X86_64, FlowArch::X86_64) | (FlowArch::Aarch64, FlowArch::Aarch64) => (),
            _ => {
                log::error!("mismatch between job arch and local arch. skipping job...");
                skipped_jobs.insert(idx);
                continue;
            }
        }

        let flow_platform = FlowPlatform::host(PipelineBackendHint::Local);
        let platform_ok = match (platform, flow_platform) {
            (FlowPlatform::Windows, FlowPlatform::Windows) => true,
            (FlowPlatform::Windows, FlowPlatform::Linux(_)) if windows_as_wsl => true,
            (FlowPlatform::Linux(_), FlowPlatform::Linux(_)) => true,
            (FlowPlatform::MacOs, FlowPlatform::MacOs) => true,
            _ => false,
        };

        if !platform_ok {
            log::error!("mismatch between job platform and local platform. skipping job...");
            log::info!("");
            if crate::running_in_wsl() && matches!(platform, FlowPlatform::Windows) {
                log::warn!("###");
                log::warn!("### NOTE: detected that you're running in WSL2");
                log::warn!(
                    "###       if the the pipeline supports it, you can try passing --windows-as-wsl"
                );
                log::warn!("###");
                log::info!("");
            }
            skipped_jobs.insert(idx);
            continue;
        }

        let nodes = {
            let mut resolved_local_steps = Vec::new();

            let (mut output_graph, _, err_unreachable_nodes) =
                crate::flow_resolver::stage1_dag::stage1_dag(
                    FlowBackend::Local,
                    flow_platform,
                    flow_arch,
                    patches.clone(),
                    root_nodes
                        .clone()
                        .into_iter()
                        .map(|(node, requests)| (node, (true, requests)))
                        .collect(),
                    external_read_vars.clone(),
                    Some(VAR_DB_SEEDVAR_FLOWEY_PERSISTENT_STORAGE_DIR.into()),
                )?;

            if err_unreachable_nodes.is_some() {
                anyhow::bail!("detected unreachable nodes")
            }

            let output_order = petgraph::algo::toposort(&output_graph, None)
                .map_err(|e| {
                    format!(
                        "includes node {}",
                        output_graph[e.node_id()].0.node.modpath()
                    )
                })
                .expect("runtime variables cannot introduce a DAG cycle");

            for idx in output_order.into_iter().rev() {
                let OutputGraphEntry { node_handle, step } = output_graph[idx].1.take().unwrap();

                let (label, code, idx, can_merge) = match step {
                    Step::Anchor { .. } => continue,
                    Step::Rust {
                        label,
                        code,
                        idx,
                        can_merge,
                    } => (label, code, idx, can_merge),
                    Step::AdoYaml { .. } => {
                        anyhow::bail!(
                            "{} emitted ADO YAML. Fix the node by checking `ctx.backend()` appropriately",
                            node_handle.modpath()
                        )
                    }
                    Step::GitHubYaml { .. } => {
                        anyhow::bail!(
                            "{} emitted GitHub YAML. Fix the node by checking `ctx.backend()` appropriately",
                            node_handle.modpath()
                        )
                    }
                };

                resolved_local_steps.push(ResolvedRunnableStep {
                    node_handle,
                    label,
                    code: code.lock().take().unwrap(),
                    idx,
                    can_merge,
                });
            }

            resolved_local_steps
        };

        let mut in_mem_var_db = crate::var_db::in_memory::InMemoryVarDb::new();

        for ResolvedJobUseParameter {
            flowey_var,
            pipeline_param_idx,
        } in parameters_used
        {
            log::trace!(
                "resolving parameter idx {}, flowey_var {:?}",
                pipeline_param_idx,
                flowey_var
            );
            let (desc, value) = match &parameters[*pipeline_param_idx] {
                Parameter::Bool {
                    name: _,
                    description,
                    kind: _,
                    default,
                } => (
                    description,
                    default.as_ref().map(|v| serde_json::to_vec(v).unwrap()),
                ),
                Parameter::String {
                    name: _,
                    description,
                    kind: _,
                    default,
                    possible_values: _,
                } => (
                    description,
                    default.as_ref().map(|v| serde_json::to_vec(v).unwrap()),
                ),
                Parameter::Num {
                    name: _,
                    description,
                    kind: _,
                    default,
                    possible_values: _,
                } => (
                    description,
                    default.as_ref().map(|v| serde_json::to_vec(v).unwrap()),
                ),
            };

            let Some(value) = value else {
                anyhow::bail!(
                    "pipeline must specify default value for params when running locally. missing default for '{desc}'"
                )
            };

            in_mem_var_db.set_var(flowey_var, false, value);
        }

        in_mem_var_db.set_var(
            VAR_DB_SEEDVAR_FLOWEY_PERSISTENT_STORAGE_DIR,
            false,
            serde_json::to_string(&persist_dir).unwrap().into(),
        );

        for ResolvedJobArtifact { flowey_var, name } in artifacts_published {
            let path = out_dir.join("artifacts").join(name);
            fs_err::create_dir_all(&path)?;

            in_mem_var_db.set_var(
                flowey_var,
                false,
                serde_json::to_string(&path).unwrap().into(),
            );
        }

        if out_dir.join(".job_artifacts").exists() {
            fs_err::remove_dir_all(out_dir.join(".job_artifacts"))?;
        }
        fs_err::create_dir_all(out_dir.join(".job_artifacts"))?;

        for ResolvedJobArtifact { flowey_var, name } in artifacts_used {
            let path = out_dir.join(".job_artifacts").join(name);
            fs_err::create_dir_all(&path)?;
            copy_dir_all(out_dir.join("artifacts").join(name), &path)?;

            in_mem_var_db.set_var(
                flowey_var,
                false,
                serde_json::to_string(&path).unwrap().into(),
            );
        }

        if out_dir.join(".work").exists() {
            fs_err::remove_dir_all(out_dir.join(".work"))?;
        }
        fs_err::create_dir_all(out_dir.join(".work"))?;

        if let Some(cond_param_idx) = cond_param_idx {
            let Parameter::Bool {
                name,
                description: _,
                kind: _,
                default: _,
            } = &parameters[cond_param_idx]
            else {
                panic!("cond param is guaranteed to be bool by type system")
            };

            // Vars should have had their default already applied, so this should never fail.
            let (data, _secret) = in_mem_var_db.get_var(name);
            let should_run: bool = serde_json::from_slice(&data).unwrap();

            if !should_run {
                log::warn!("job condition was false - skipping job...");
                skipped_jobs.insert(idx);
                continue;
            }
        }

        let mut runtime_services = flowey_core::node::steps::rust::new_rust_runtime_services(
            &mut in_mem_var_db,
            FlowBackend::Local,
            platform,
            flow_arch,
        )?;

        for ResolvedRunnableStep {
            node_handle,
            label,
            code,
            idx,
            can_merge,
        } in nodes
        {
            let node_working_dir = out_dir.join(".work").join(format!(
                "{}_{}",
                node_handle.modpath().replace("::", "__"),
                idx
            ));
            if !node_working_dir.exists() {
                fs_err::create_dir(&node_working_dir)?;
            }

            std::env::set_current_dir(node_working_dir.clone())?;
            runtime_services.sh.change_dir(node_working_dir);

            if can_merge {
                log::debug!("minor step: {} ({})", label, node_handle.modpath(),);
            } else {
                log::info!(
                    // green color
                    "\x1B[0;32m=== {} ({}) ===\x1B[0m",
                    label,
                    node_handle.modpath(),
                );
            }
            code(&mut runtime_services)?;
            if can_merge {
                log::debug!("done!");
                log::debug!(""); // log a newline, for the pretty
            } else {
                log::info!("\x1B[0;32m=== done! ===\x1B[0m");
                log::info!(""); // log a newline, for the pretty
            }
        }

        // Leave the last node's working dir so it can be deleted by later steps
        std::env::set_current_dir(&out_dir)?;
    }

    Ok(())
}

fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    fs_err::create_dir_all(&dst)?;
    for entry in fs_err::read_dir(src.as_ref())? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs_err::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}
