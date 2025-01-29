// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared functionality for emitting a pipeline as ADO/GitHub YAML files

use crate::cli::exec_snippet::FloweyPipelineStaticDb;
use crate::cli::pipeline::CheckMode;
use crate::pipeline_resolver::generic::ResolvedPipelineJob;
use anyhow::Context;
use flowey_core::node::FlowArch;
use flowey_core::node::FlowPlatform;
use petgraph::visit::EdgeRef;
use serde::Serialize;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

#[derive(Debug)]
pub(crate) enum FloweySource {
    // bool indicates if this node should publish the flowey it bootstraps for
    // other nodes to consume
    Bootstrap(String, bool),
    Consume(String),
}

/// each job has one of three "roles" when it comes to bootstrapping flowey:
///
/// 1. Build flowey
/// 2. Building _and_ publishing flowey
/// 3. Consuming a pre-built flowey
///
/// We _could_ just have every bootstrap job also publish flowey, but this
/// will spam the artifact feed with artifacts no one will consume, which is
/// wasteful.
///
/// META: why go through all this hassle anyways? i.e: why not just do
/// something dead simple like:
///
/// - discover which platforms exist in the graph
/// - have the first jobs of every pipeline be standalone "bootstrap flowey"
///   jobs, which all subsequent jobs of a certain platform can take a dep on
///
/// well... it turns out that provisioning job runners is _sloooooow_,
/// and having every single pipeline run these "bootstrap flowey" steps
/// gating the rest of the "interesting" stuff would really stink.
///
/// i.e: it's better to do redundant flowey bootstraps if it means that we
/// can avoid the extra time it takes to tear down + re-provision a worker.
pub(crate) fn job_flowey_bootstrap_source(
    graph: &petgraph::Graph<ResolvedPipelineJob, ()>,
    order: &Vec<petgraph::prelude::NodeIndex>,
) -> BTreeMap<petgraph::prelude::NodeIndex, FloweySource> {
    let mut bootstrapped_flowey = BTreeMap::new();

    // the first traversal builds a list of all ancestors of a give node
    let mut ancestors = BTreeMap::<
        petgraph::prelude::NodeIndex,
        BTreeSet<(petgraph::prelude::NodeIndex, FlowPlatform, FlowArch)>,
    >::new();
    for idx in order {
        for ancestor_idx in graph
            .edges_directed(*idx, petgraph::Direction::Incoming)
            .map(|e| e.source())
        {
            ancestors.entry(*idx).or_default().insert((
                ancestor_idx,
                graph[ancestor_idx].platform,
                graph[ancestor_idx].arch,
            ));

            if let Some(set) = ancestors.get(&ancestor_idx).cloned() {
                ancestors.get_mut(idx).unwrap().extend(&set);
            }
        }
    }

    // the second traversal assigns roles to each node
    let mut floweyno = 0;
    'outer: for idx in order {
        let ancestors = ancestors.remove(idx).unwrap_or_default();

        let mut elect_bootstrap = None;

        for (ancestor_idx, platform, arch) in ancestors {
            if platform != graph[*idx].platform || arch != graph[*idx].arch {
                continue;
            }

            let role =
                bootstrapped_flowey
                    .get_mut(&ancestor_idx)
                    .and_then(|existing| match existing {
                        FloweySource::Bootstrap(s, true) => Some(FloweySource::Consume(s.clone())),
                        FloweySource::Consume(s) => Some(FloweySource::Consume(s.clone())),
                        // there is an ancestor that is building, but not
                        // publishing. maybe they should get upgraded...
                        FloweySource::Bootstrap(_, false) => {
                            elect_bootstrap = Some(ancestor_idx);
                            None
                        }
                    });

            if let Some(role) = role {
                bootstrapped_flowey.insert(*idx, role);
                continue 'outer;
            }
        }

        // if we got here, that means we couldn't find a valid ancestor.
        //
        // check if we can upgrade an existing ancestor vs. bootstrapping
        // things ourselves
        if let Some(elect_bootstrap) = elect_bootstrap {
            let FloweySource::Bootstrap(s, publish) =
                bootstrapped_flowey.get_mut(&elect_bootstrap).unwrap()
            else {
                unreachable!()
            };

            *publish = true;
            let s = s.clone();

            bootstrapped_flowey.insert(*idx, FloweySource::Consume(s));
        } else {
            // Having this extra unique `floweyno` per bootstrap is
            // necessary since GitHub doesn't let you double-publish an
            // artifact with the same name
            floweyno += 1;
            let platform = graph[*idx].platform;
            let arch = graph[*idx].arch;
            bootstrapped_flowey.insert(
                *idx,
                FloweySource::Bootstrap(
                    format!("_internal-flowey-bootstrap-{arch}-{platform}-uid-{floweyno}"),
                    false,
                ),
            );
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
