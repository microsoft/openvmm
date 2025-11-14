// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use flowey_core::node::FlowArch;
use flowey_core::node::FlowPlatform;
use flowey_core::node::NodeHandle;
use flowey_core::node::user_facing::GhPermission;
use flowey_core::node::user_facing::GhPermissionValue;
use flowey_core::patch::ResolvedPatches;
use flowey_core::pipeline::AdoCiTriggers;
use flowey_core::pipeline::AdoPrTriggers;
use flowey_core::pipeline::AdoScheduleTriggers;
use flowey_core::pipeline::GhCiTriggers;
use flowey_core::pipeline::GhPrTriggers;
use flowey_core::pipeline::GhRunner;
use flowey_core::pipeline::GhScheduleTriggers;
use flowey_core::pipeline::Pipeline;
use flowey_core::pipeline::internal::AdoPool;
use flowey_core::pipeline::internal::ArtifactMeta;
use flowey_core::pipeline::internal::InternalAdoResourcesRepository;
use flowey_core::pipeline::internal::Parameter;
use flowey_core::pipeline::internal::ParameterMeta;
use flowey_core::pipeline::internal::PipelineFinalized;
use flowey_core::pipeline::internal::PipelineJobMetadata;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub struct ResolvedPipeline {
    pub graph: petgraph::Graph<ResolvedPipelineJob, ()>,
    pub order: Vec<petgraph::prelude::NodeIndex>,
    pub parameters: Vec<Parameter>,
    pub ado_schedule_triggers: Vec<AdoScheduleTriggers>,
    pub ado_name: Option<String>,
    pub ado_ci_triggers: Option<AdoCiTriggers>,
    pub ado_pr_triggers: Option<AdoPrTriggers>,
    pub ado_bootstrap_template: String,
    pub ado_resources_repository: Vec<InternalAdoResourcesRepository>,
    pub ado_post_process_yaml_cb: Option<Box<dyn FnOnce(serde_yaml::Value) -> serde_yaml::Value>>,
    pub ado_variables: BTreeMap<String, String>,
    pub ado_job_id_overrides: BTreeMap<usize, String>,
    pub gh_name: Option<String>,
    pub gh_schedule_triggers: Vec<GhScheduleTriggers>,
    pub gh_ci_triggers: Option<GhCiTriggers>,
    pub gh_pr_triggers: Option<GhPrTriggers>,
    pub gh_bootstrap_template: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedJobArtifact {
    pub flowey_var: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedJobUseParameter {
    pub flowey_var: String,
    pub pipeline_param_idx: usize,
}

#[derive(Debug, Clone)] // Clone is because of shoddy viz code
pub struct ResolvedPipelineJob {
    pub root_nodes: BTreeMap<NodeHandle, Vec<Box<[u8]>>>,
    pub patches: ResolvedPatches,
    pub label: String,
    pub platform: FlowPlatform,
    pub arch: FlowArch,
    pub ado_pool: Option<AdoPool>,
    pub timeout_minutes: Option<u32>,
    pub ado_variables: BTreeMap<String, String>,
    pub gh_override_if: Option<String>,
    pub gh_global_env: BTreeMap<String, String>,
    pub gh_pool: Option<GhRunner>,
    pub gh_permissions: BTreeMap<NodeHandle, BTreeMap<GhPermission, GhPermissionValue>>,
    pub external_read_vars: BTreeSet<String>,
    pub cond_param_idx: Option<usize>,

    pub parameters_used: Vec<ResolvedJobUseParameter>,
    // correspond to injected download nodes at the start of the job
    pub artifacts_used: Vec<ResolvedJobArtifact>,
    // correspond to injected publish nodes at the end of the job
    pub artifacts_published: Vec<ResolvedJobArtifact>,
}

pub fn resolve_pipeline(pipeline: Pipeline) -> anyhow::Result<ResolvedPipeline> {
    let PipelineFinalized {
        jobs,
        artifacts,
        parameters,
        extra_deps,
        ado_name,
        ado_schedule_triggers,
        ado_ci_triggers,
        ado_pr_triggers,
        ado_bootstrap_template,
        ado_resources_repository,
        ado_post_process_yaml_cb,
        ado_variables,
        ado_job_id_overrides,
        gh_name,
        gh_schedule_triggers,
        gh_ci_triggers,
        gh_pr_triggers,
        gh_bootstrap_template,
    } = PipelineFinalized::from_pipeline(pipeline);

    let mut graph = petgraph::Graph::new();

    let mut job_to_artifacts = {
        let mut m = BTreeMap::<usize, (BTreeSet<String>, BTreeSet<String>)>::new();

        for ArtifactMeta {
            name,
            published_by_job,
            used_by_jobs,
        } in &artifacts
        {
            let no_existing = m
                .entry(
                    published_by_job
                        .context(format!("artifact '{name}' is not published by any job"))?,
                )
                .or_default()
                .0
                .insert(name.clone());
            assert!(no_existing);

            for job_idx in used_by_jobs {
                let no_existing = m.entry(*job_idx).or_default().1.insert(name.clone());
                assert!(no_existing);
            }
        }

        m
    };

    let (parameters, mut job_to_params) = {
        let mut params = Vec::new();
        let mut m = BTreeMap::<usize, BTreeSet<usize>>::new();

        for (
            param_idx,
            ParameterMeta {
                parameter,
                used_by_jobs,
            },
        ) in parameters.into_iter().enumerate()
        {
            params.push(parameter);
            for job_idx in used_by_jobs {
                let no_existing = m.entry(job_idx).or_default().insert(param_idx);
                assert!(no_existing);
            }
        }

        (params, m)
    };

    let mut flowey_bootstrap_platforms = BTreeSet::new();

    // first things first: spin up graph nodes for each job
    let mut job_graph_idx = Vec::new();
    for (
        job_idx,
        PipelineJobMetadata {
            root_nodes,
            patches,
            label,
            platform,
            arch,
            cond_param_idx,
            timeout_minutes,
            ado_pool,
            ado_variables,
            gh_override_if,
            gh_global_env,
            gh_pool,
            gh_permissions,
        },
    ) in jobs.into_iter().enumerate()
    {
        let (artifacts_published, artifacts_used) =
            job_to_artifacts.remove(&job_idx).unwrap_or_default();
        let parameters_used = job_to_params.remove(&job_idx).unwrap_or_default();

        let artifacts_published: Vec<_> = artifacts_published
            .into_iter()
            .map(|a| ResolvedJobArtifact {
                flowey_var: flowey_core::pipeline::internal::consistent_artifact_runtime_var_name(
                    &a, false,
                ),
                name: a,
            })
            .collect();
        let artifacts_used: Vec<_> = artifacts_used
            .into_iter()
            .map(|a| ResolvedJobArtifact {
                flowey_var: flowey_core::pipeline::internal::consistent_artifact_runtime_var_name(
                    &a, true,
                ),
                name: a,
            })
            .collect();
        let parameters_used: Vec<_> = parameters_used
            .into_iter()
            .map(|param_idx| ResolvedJobUseParameter {
                flowey_var: parameters[param_idx].name().to_string(),
                pipeline_param_idx: param_idx,
            })
            .collect();

        // individual pipeline resolvers still need to ensure that the var is in
        // the var-db at job start time, but this external-var reporting code
        // can be shared across all impls
        let mut external_read_vars = BTreeSet::new();
        external_read_vars.extend(artifacts_used.iter().map(|a| a.flowey_var.clone()));
        external_read_vars.extend(artifacts_published.iter().map(|a| a.flowey_var.clone()));
        external_read_vars.extend(parameters_used.iter().map(|p| p.flowey_var.clone()));

        let idx = graph.add_node(ResolvedPipelineJob {
            root_nodes,
            patches: patches.finalize(),
            label,
            timeout_minutes,
            ado_pool,
            ado_variables,
            gh_override_if,
            gh_global_env,
            gh_pool,
            gh_permissions,
            platform,
            arch,
            cond_param_idx,
            external_read_vars,
            parameters_used,
            artifacts_used,
            artifacts_published,
        });

        // ...also using this opportunity to keep track of what flowey bins we need to bootstrap
        flowey_bootstrap_platforms.insert(platform);

        job_graph_idx.push(idx);
    }

    // next, add node edges based on artifact flow
    for ArtifactMeta {
        name: _,
        published_by_job,
        used_by_jobs,
    } in artifacts
    {
        let published_idx = job_graph_idx[published_by_job.expect("checked in loop above")];
        for job in used_by_jobs {
            let used_idx = job_graph_idx[job];
            graph.add_edge(published_idx, used_idx, ());
        }
    }

    // lastly, add node edges based on any additional explicit dependencies
    for (from, to) in extra_deps {
        graph.add_edge(job_graph_idx[from], job_graph_idx[to], ());
    }

    // TODO: better error handling
    let order = petgraph::algo::toposort(&graph, None)
        .map_err(|_| anyhow::anyhow!("detected cycle in pipeline"))?;

    Ok(ResolvedPipeline {
        graph,
        order,
        parameters,
        ado_name,
        ado_variables,
        ado_schedule_triggers,
        ado_ci_triggers,
        ado_pr_triggers,
        ado_bootstrap_template,
        ado_resources_repository,
        ado_post_process_yaml_cb,
        ado_job_id_overrides,
        gh_name,
        gh_schedule_triggers,
        gh_ci_triggers,
        gh_pr_triggers,
        gh_bootstrap_template,
    })
}

impl ResolvedPipeline {
    /// Trim the pipeline graph to only include the specified jobs (taking care
    /// to also preserve any dependant jobs they rely on).
    pub fn trim_pipeline_graph(&mut self, preserve_jobs: Vec<petgraph::prelude::NodeIndex>) {
        // DEVNOTE: this is a horribly suboptimal way to implement this, but it
        // works fine with the graph-sizes we currently have, so we can optimize
        // this later...

        let mut jobs_to_delete: BTreeSet<_> = self.graph.node_indices().collect();
        for idx in preserve_jobs {
            let g = petgraph::visit::Reversed(&self.graph);

            let mut dfs = petgraph::visit::Dfs::new(g, idx);
            while let Some(save_idx) = dfs.next(g) {
                jobs_to_delete.remove(&save_idx);
            }
        }

        let mut jobs_to_delete = jobs_to_delete.into_iter().collect::<Vec<_>>();
        jobs_to_delete.sort();

        // in petgraph, when you remove a node, it invalidates the node idx of
        // all subsequent nodes.
        //
        // I'm sure there's a better way to do this filtering, but just removing
        // nodes in reverse order seems to work fine.
        for idx in jobs_to_delete.into_iter().rev() {
            self.graph.remove_node(idx).unwrap();
        }

        self.order = petgraph::algo::toposort(&self.graph, None).unwrap();
    }
}
