// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared functionality for emitting a pipeline as ADO/GitHub YAML files

use crate::cli::exec_snippet::FloweyPipelineStaticDb;
use crate::cli::pipeline::CheckMode;
use crate::pipeline_resolver::generic::ResolvedPipelineJob;
use anyhow::Context;
use flowey_core::node::FlowArch;
use flowey_core::node::FlowPlatform;
use serde::Serialize;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

#[derive(Debug)]
pub(crate) enum FloweySource {
    // bool indicates if this node should publish the flowey it bootstraps for
    // other nodes to consume
    Bootstrap(String, bool),
    Consume(String),
}

/// Adds edges to the graph to ensure all jobs on the same platform/arch depend on
/// a single designated "bootstrap" job for that platform. This allows flowey to be
/// built once per platform and reused by all other jobs.
///
/// Returns a map of (platform, arch) -> bootstrap job index
pub(crate) fn add_flowey_bootstrap_dependencies(
    graph: &mut petgraph::Graph<ResolvedPipelineJob, ()>,
) -> BTreeMap<(FlowPlatform, FlowArch), petgraph::prelude::NodeIndex> {
    let mut platform_publishers: BTreeMap<(FlowPlatform, FlowArch), petgraph::prelude::NodeIndex> =
        BTreeMap::new();

    // First, identify the best bootstrap job for each platform/arch
    // We want jobs with the fewest dependencies (ideally zero)
    for idx in graph.node_indices() {
        let platform = graph[idx].platform;
        let arch = graph[idx].arch;

        let dependency_count = graph
            .edges_directed(idx, petgraph::Direction::Incoming)
            .count();

        platform_publishers
            .entry((platform, arch))
            .and_modify(|current_idx| {
                let current_dep_count = graph
                    .edges_directed(*current_idx, petgraph::Direction::Incoming)
                    .count();

                if dependency_count < current_dep_count {
                    *current_idx = idx;
                }
            })
            .or_insert(idx);
    }

    // Second, add edges from each bootstrap job to all other jobs on the same platform
    // (unless there's already a path between them)
    for idx in graph.node_indices() {
        let platform = graph[idx].platform;
        let arch = graph[idx].arch;
        let bootstrap_idx = platform_publishers[&(platform, arch)];

        // Skip the bootstrap job itself
        if idx == bootstrap_idx {
            continue;
        }

        // Check if there's already a path from bootstrap to this job
        let path_exists = petgraph::algo::has_path_connecting(&*graph, bootstrap_idx, idx, None);

        if !path_exists {
            // Add edge from bootstrap job to this job
            graph.add_edge(bootstrap_idx, idx, ());
        }
    }

    platform_publishers
}

/// each job has one of three "roles" when it comes to bootstrapping flowey:
///
/// 1. Build flowey
/// 2. Building _and_ publishing flowey
/// 3. Consuming a pre-built flowey
///
/// Strategy: The designated bootstrap job for each platform/arch will build and
/// publish flowey. All other jobs on that platform will consume it.
///
/// This function should be called AFTER add_flowey_bootstrap_dependencies() has
/// modified the graph to ensure proper dependency ordering.
pub(crate) fn job_flowey_bootstrap_source(
    graph: &petgraph::Graph<ResolvedPipelineJob, ()>,
    order: &Vec<petgraph::prelude::NodeIndex>,
    platform_publishers: &BTreeMap<(FlowPlatform, FlowArch), petgraph::prelude::NodeIndex>,
) -> BTreeMap<petgraph::prelude::NodeIndex, FloweySource> {
    let mut bootstrapped_flowey = BTreeMap::new();

    // Assign roles: publishers build and publish, all others consume
    let mut floweyno = 0;
    let mut published_artifacts: BTreeMap<(FlowPlatform, FlowArch), String> = BTreeMap::new();

    for idx in order {
        let platform = graph[*idx].platform;
        let arch = graph[*idx].arch;
        let publisher_idx = platform_publishers[&(platform, arch)];

        if *idx == publisher_idx {
            // This is the designated publisher for this platform/arch
            let artifact = format!("flowey_{floweyno}_{}", graph[*idx].label.replace(' ', "_"));
            floweyno += 1;
            published_artifacts.insert((platform, arch), artifact.clone());
            bootstrapped_flowey.insert(*idx, FloweySource::Bootstrap(artifact, true));
        } else {
            // This job consumes from the platform publisher
            // Since we've already added the dependency in add_flowey_bootstrap_dependencies,
            // we know the publisher is an ancestor
            let artifact = published_artifacts[&(platform, arch)].clone();
            bootstrapped_flowey.insert(*idx, FloweySource::Consume(artifact));
        }
    }

    bootstrapped_flowey
}

/// convert `pipeline` to YAML and `pipeline_static_db` to JSON.
/// if `check` is `Some`, then we will compare the generated YAML and JSON
/// against the contents of `check` and error if they don't match.
/// if `check` is `None`, then we will write the generated YAML and JSON to
/// `repo_root/pipeline_file.yaml` and `repo_root/pipeline_file.json` respectively.
fn check_or_write_generated_yaml_and_json<T>(
    pipeline: &T,
    pipeline_static_db: &FloweyPipelineStaticDb,
    mode: CheckMode,
    repo_root: &Path,
    pipeline_file: &Path,
    ado_post_process_yaml_cb: Option<Box<dyn FnOnce(Value) -> Value>>,
) -> anyhow::Result<()>
where
    T: Serialize,
{
    let generated_yaml =
        serde_yaml::to_value(pipeline).context("while serializing pipeline yaml")?;
    let generated_yaml = if let Some(ado_post_process_yaml_cb) = ado_post_process_yaml_cb {
        ado_post_process_yaml_cb(generated_yaml)
    } else {
        generated_yaml
    };

    let generated_yaml =
        serde_yaml::to_string(&generated_yaml).context("while emitting pipeline yaml")?;
    let generated_yaml = format!(
        r#"
##############################
# THIS FILE IS AUTOGENERATED #
#    DO NOT MANUALLY EDIT    #
##############################
{generated_yaml}"#
    );
    let generated_yaml = generated_yaml.trim_start();

    let generated_json =
        serde_json::to_string_pretty(pipeline_static_db).context("while emitting pipeline json")?;

    match mode {
        CheckMode::Runtime(ref check_file) | CheckMode::Check(ref check_file) => {
            let existing_yaml = fs_err::read_to_string(check_file)
                .context("cannot check pipeline that doesn't exist!")?;

            let yaml_out_of_date = existing_yaml != generated_yaml;

            if yaml_out_of_date {
                println!(
                    "generated yaml {}:\n==========\n{generated_yaml}",
                    generated_yaml.len()
                );
                println!(
                    "existing yaml {}:\n==========\n{existing_yaml}",
                    existing_yaml.len()
                );
            }

            if yaml_out_of_date {
                anyhow::bail!("checked in pipeline YAML is out of date! run `cargo xflowey regen`")
            }

            // Only write the JSON if we're in runtime mode, not in check mode
            if let CheckMode::Runtime(_) = mode {
                let mut f = fs_err::File::create(check_file.with_extension("json"))?;
                f.write_all(generated_json.as_bytes())
                    .context("while emitting pipeline database json")?;
            }

            Ok(())
        }
        CheckMode::None => {
            let out_yaml_path = repo_root.join(pipeline_file);

            let mut f = fs_err::File::create(out_yaml_path)?;
            f.write_all(generated_yaml.as_bytes())
                .context("while emitting pipeline yaml")?;

            Ok(())
        }
    }
}

/// See [`check_or_write_generated_yaml_and_json`]
pub(crate) fn check_generated_yaml_and_json<T>(
    pipeline: &T,
    pipeline_static_db: &FloweyPipelineStaticDb,
    check: CheckMode,
    repo_root: &Path,
    pipeline_file: &Path,
    ado_post_process_yaml_cb: Option<Box<dyn FnOnce(Value) -> Value>>,
) -> anyhow::Result<()>
where
    T: Serialize,
{
    check_or_write_generated_yaml_and_json(
        pipeline,
        pipeline_static_db,
        check,
        repo_root,
        pipeline_file,
        ado_post_process_yaml_cb,
    )
}

/// See [`check_or_write_generated_yaml_and_json`]
pub(crate) fn write_generated_yaml_and_json<T>(
    pipeline: &T,
    pipeline_static_db: &FloweyPipelineStaticDb,
    repo_root: &Path,
    pipeline_file: &Path,
    ado_post_process_yaml_cb: Option<Box<dyn FnOnce(Value) -> Value>>,
) -> anyhow::Result<()>
where
    T: Serialize,
{
    check_or_write_generated_yaml_and_json(
        pipeline,
        pipeline_static_db,
        CheckMode::None,
        repo_root,
        pipeline_file,
        ado_post_process_yaml_cb,
    )
}

/// Merges a list of bash commands into a single YAML step.
pub(crate) struct BashCommands {
    commands: Vec<String>,
    label: Option<String>,
    can_merge: bool,
    github: bool,
}

impl BashCommands {
    pub fn new_github() -> Self {
        Self {
            commands: Vec::new(),
            label: None,
            can_merge: true,
            github: true,
        }
    }

    pub fn new_ado() -> Self {
        Self {
            commands: Vec::new(),
            label: None,
            can_merge: true,
            github: false,
        }
    }

    #[must_use]
    pub fn push(
        &mut self,
        label: Option<String>,
        can_merge: bool,
        mut cmd: String,
    ) -> Option<Value> {
        let val = if !can_merge && !self.can_merge {
            self.flush()
        } else {
            None
        };
        if !can_merge || self.label.is_none() {
            self.label = label;
        }
        cmd.truncate(cmd.trim_end().len());
        self.commands.push(cmd);
        self.can_merge &= can_merge;
        val
    }

    pub fn push_minor(&mut self, cmd: String) {
        assert!(self.push(None, true, cmd).is_none());
    }

    #[must_use]
    pub fn flush(&mut self) -> Option<Value> {
        if self.commands.is_empty() {
            return None;
        }
        let label = if self.commands.len() == 1 || !self.can_merge {
            self.label.take()
        } else {
            None
        };
        let label = label.unwrap_or_else(|| "🦀 flowey rust steps".into());
        let map = if self.github {
            let commands = self.commands.join("\n");
            serde_yaml::Mapping::from_iter([
                ("name".into(), label.into()),
                ("run".into(), commands.into()),
                ("shell".into(), "bash".into()),
            ])
        } else {
            let commands = if self.commands.len() == 1 {
                self.commands.drain(..).next().unwrap()
            } else {
                // ADO doesn't automatically fail on error on multi-line scripts.
                self.commands.insert(0, "set -e".into());
                self.commands.join("\n")
            };
            serde_yaml::Mapping::from_iter([
                ("bash".into(), commands.into()),
                ("displayName".into(), label.into()),
            ])
        };
        self.commands.clear();
        self.can_merge = true;
        Some(map.into())
    }
}
