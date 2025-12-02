// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Debug backend that simply visualizes flows, instead of emitting them in any
//! runnable format

use crate::flow_resolver::stage1_dag::DepKind;
use crate::flow_resolver::stage1_dag::OutputGraphEntry;
use crate::flow_resolver::stage1_dag::StepId;
use crate::pipeline_resolver::generic::ResolvedPipeline;
use crate::pipeline_resolver::generic::ResolvedPipelineJob;
use flowey_core::node::FlowArch;
use flowey_core::node::FlowBackend;
use flowey_core::node::FlowPlatform;
use flowey_core::node::NodeHandle;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub fn viz_pipeline_toposort(
    pipeline: ResolvedPipeline,
    backend: FlowBackend,
    with_persist_dir: bool,
) -> anyhow::Result<()> {
    viz_pipeline_generic(pipeline, backend, with_persist_dir, viz_flow_toposort)
}

pub fn viz_pipeline_flow_dot(
    pipeline: ResolvedPipeline,
    backend: FlowBackend,
    with_persist_dir: bool,
) -> anyhow::Result<()> {
    viz_pipeline_generic(pipeline, backend, with_persist_dir, viz_flow_dot)
}

fn viz_pipeline_generic(
    pipeline: ResolvedPipeline,
    backend: FlowBackend,
    with_persist_dir: bool,
    f: fn(
        seed_nodes: BTreeMap<NodeHandle, (bool, Vec<Box<[u8]>>)>,
        resolved_patches: flowey_core::patch::ResolvedPatches,
        external_read_vars: BTreeSet<String>,
        backend: FlowBackend,
        platform: FlowPlatform,
        arch: FlowArch,
        with_persist_dir: bool,
    ) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let ResolvedPipeline {
        graph,
        order,
        parameters: _,
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

    for idx in order {
        let ResolvedPipelineJob {
            ref root_nodes,
            ref patches,
            ref label,
            platform,
            arch,
            cond_param_idx: _,
            timeout_minutes: _,
            ref ado_pool,
            ado_variables: _,
            gh_override_if: _,
            gh_global_env: _,
            ref gh_pool,
            gh_permissions: _,
            ref external_read_vars,
            parameters_used: _,
            ref artifacts_used,
            ref artifacts_published,
        } = graph[idx];

        println!(
            "== {}{}{} ==",
            label,
            ado_pool
                .as_ref()
                .map(|s| format!(" - {} ({})", s.name, s.demands.join(",")))
                .unwrap_or_default(),
            gh_pool
                .as_ref()
                .map(|s| format!(" - {:#?}", s))
                .unwrap_or_default()
        );
        println!(
            "artifacts used: {}",
            artifacts_used
                .iter()
                .map(|a| a.name.clone())
                .collect::<Vec<_>>()
                .join(",\n    ")
        );
        println!(
            "artifacts published: {}",
            artifacts_published
                .iter()
                .map(|a| a.name.clone())
                .collect::<Vec<_>>()
                .join(",\n    ")
        );
        println!();

        f(
            root_nodes
                .clone()
                .into_iter()
                .map(|(node, requests)| (node, (true, requests)))
                .collect(),
            patches.clone(),
            external_read_vars.clone(),
            backend,
            platform,
            arch,
            with_persist_dir,
        )?;

        println!();
    }

    Ok(())
}

/// (debug) print the modpath of each node in topological sort of the flow
pub fn viz_flow_toposort(
    seed_nodes: BTreeMap<NodeHandle, (bool, Vec<Box<[u8]>>)>,
    resolved_patches: flowey_core::patch::ResolvedPatches,
    external_read_vars: BTreeSet<String>,
    backend: FlowBackend,
    platform: FlowPlatform,
    arch: FlowArch,
    with_persist_dir: bool,
) -> anyhow::Result<()> {
    // ignore the unreachable nodes error, since we want to allow debugging issues here
    let (mut output_graph, _, _err_unreachable_nodes) =
        crate::flow_resolver::stage1_dag::stage1_dag(
            backend,
            platform,
            arch,
            resolved_patches,
            seed_nodes,
            external_read_vars,
            with_persist_dir.then_some("<dummy>".into()),
        )?;

    let output_order = petgraph::algo::toposort(&output_graph, None)
        .expect("runtime variables cannot introduce a DAG cycle");

    let mut max_len = 0;
    for &idx in output_order.iter().rev() {
        max_len = max_len.max(
            output_graph[idx]
                .1
                .as_ref()
                .unwrap()
                .node_handle
                .modpath()
                .len(),
        )
    }

    for idx in output_order.into_iter().rev() {
        let e = output_graph[idx].1.take().unwrap();
        match &e.step {
            crate::flow_resolver::stage1_dag::Step::Anchor { .. } => {}
            crate::flow_resolver::stage1_dag::Step::Rust {
                idx: _,
                label,
                can_merge: _,
                code: _,
            } => {
                println!(
                    "{:width$} - rust - {}",
                    e.node_handle.modpath(),
                    label,
                    width = max_len
                )
            }
            crate::flow_resolver::stage1_dag::Step::AdoYaml {
                ado_to_rust: _,
                rust_to_ado: _,
                label,
                raw_yaml: _,
                condvar: _,
                code_idx: _,
                code: _,
            } => println!(
                "{:width$} - ado  - {}",
                e.node_handle.modpath(),
                label,
                width = max_len
            ),
            crate::flow_resolver::stage1_dag::Step::GitHubYaml { label, .. } => println!(
                "{:width$} - github - {}",
                e.node_handle.modpath(),
                label,
                width = max_len
            ),
        }
    }

    Ok(())
}

pub fn viz_pipeline_dot(pipeline: ResolvedPipeline, _backend: FlowBackend) -> anyhow::Result<()> {
    let ResolvedPipeline {
        graph,
        order: _,
        parameters: _,
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

    #[derive(Clone)]
    struct VizNode(ResolvedPipelineJob);

    impl From<ResolvedPipelineJob> for VizNode {
        fn from(value: ResolvedPipelineJob) -> Self {
            Self(value)
        }
    }

    impl std::fmt::Debug for VizNode {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let Self(ResolvedPipelineJob {
                root_nodes: _,
                patches: _,
                label,
                platform: _,
                arch: _,
                cond_param_idx: _,
                timeout_minutes: _,
                ado_pool,
                ado_variables: _,
                gh_override_if: _,
                gh_global_env: _,
                gh_pool,
                gh_permissions: _,
                external_read_vars: _,
                parameters_used: _,
                artifacts_used,
                artifacts_published,
            }) = self;

            writeln!(
                f,
                "== {}{}{} ==",
                label,
                ado_pool
                    .as_ref()
                    .map(|s| format!(" - {} ({})", s.name, s.demands.join(",")))
                    .unwrap_or_default(),
                gh_pool
                    .as_ref()
                    .map(|s| format!(" - {:#?}", s))
                    .unwrap_or_default()
            )?;

            writeln!(
                f,
                "artifacts used: {}",
                artifacts_used
                    .iter()
                    .map(|a| a.name.clone())
                    .collect::<Vec<_>>()
                    .join(",\n    ")
            )?;
            writeln!(
                f,
                "artifacts published: {}",
                artifacts_published
                    .iter()
                    .map(|a| a.name.clone())
                    .collect::<Vec<_>>()
                    .join(",\n    ")
            )?;

            Ok(())
        }
    }

    println!(
        "{:?}",
        petgraph::dot::Dot::with_config(
            &petgraph_viz_helper::clone_graph_with_wrappers::<_, _, VizNode, ()>(&graph),
            &[petgraph::dot::Config::EdgeNoLabel]
        )
    );

    Ok(())
}

/// (debug) emit a graph in the graphviz `.dot` format of the flow
pub fn viz_flow_dot(
    seed_nodes: BTreeMap<NodeHandle, (bool, Vec<Box<[u8]>>)>,
    resolved_patches: flowey_core::patch::ResolvedPatches,
    external_read_vars: BTreeSet<String>,
    backend: FlowBackend,
    platform: FlowPlatform,
    arch: FlowArch,
    with_persist_dir: bool,
) -> anyhow::Result<()> {
    // ignore the unreachable nodes error, since we want to allow debugging issues here
    let (output_graph, _, _err_unreachable_nodes) = crate::flow_resolver::stage1_dag::stage1_dag(
        backend,
        platform,
        arch,
        resolved_patches,
        seed_nodes,
        external_read_vars,
        with_persist_dir.then_some("<dummy>".into()),
    )?;

    #[derive(Clone)]
    struct VizNode((StepId, Option<OutputGraphEntry>));

    impl From<(StepId, Option<OutputGraphEntry>)> for VizNode {
        fn from(value: (StepId, Option<OutputGraphEntry>)) -> Self {
            Self(value)
        }
    }

    impl std::fmt::Debug for VizNode {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if self.0.1.is_none() {
                return write!(f, "{:?} - ???", self.0.0);
            }

            let entry = &self.0.1.as_ref().unwrap();

            write!(
                f,
                "{}:{}\n\n{}",
                self.0.0.step_idx,
                self.0.0.node.modpath(),
                match &entry.step {
                    crate::flow_resolver::stage1_dag::Step::Anchor { label } => {
                        format!("<anchor:{label}>")
                    }
                    crate::flow_resolver::stage1_dag::Step::Rust {
                        idx,
                        label,
                        can_merge: _,
                        code: _,
                    } => format!("rust{idx}\n\n{}", label),
                    crate::flow_resolver::stage1_dag::Step::AdoYaml {
                        ado_to_rust: _,
                        rust_to_ado: _,
                        label,
                        raw_yaml: _,
                        condvar: _,
                        code_idx: _,
                        code: _,
                    } => format!("ado\n\n{}", label),
                    crate::flow_resolver::stage1_dag::Step::GitHubYaml { label, .. } => {
                        format!("github\n\n{}", label)
                    }
                }
            )
        }
    }

    println!(
        r#"
digraph {{
    #rankdir="LR"
    edge [dir="back"];
{:?}
}}
"#,
        // petgraph::dot::Dot::with_config(
        //     &petgraph::visit::Reversed(&self::petgraph_viz_helper::clone_graph_with_wrappers::<
        //         _,
        //         _,
        //         VizNode,
        //         DepKind,
        //     >(&output_graph)),
        //     &[petgraph::dot::Config::GraphContentOnly]
        // ),
        petgraph::dot::Dot::with_config(
            &petgraph_viz_helper::clone_graph_with_wrappers::<_, _, VizNode, DepKind>(
                &output_graph
            ),
            &[petgraph::dot::Config::GraphContentOnly]
        )
    );

    match petgraph::algo::toposort(&output_graph, None) {
        Ok(order) => {
            if order
                .into_iter()
                .filter(|idx| {
                    output_graph
                        .edges_directed(*idx, petgraph::Direction::Incoming)
                        .count()
                        == 0
                })
                .count()
                != 1
            {
                println!("multiple root nodes detected!")
            }
        }
        Err(_) => {
            println!("Detected Cycle!")
        }
    }

    Ok(())
}

// pub(crate) so dump_stage0_dag can use it
pub(crate) mod petgraph_viz_helper {
    use petgraph::visit::EdgeRef;
    use std::collections::BTreeMap;

    // thanks bing AI!
    pub fn clone_graph_with_wrappers<N, E, NWrap, EWrap>(
        graph: &petgraph::Graph<N, E>,
    ) -> petgraph::Graph<NWrap, EWrap>
    where
        N: Clone,
        E: Clone,
        NWrap: From<N>,
        EWrap: From<E>,
    {
        let mut new_graph = petgraph::Graph::new();
        let node_map: BTreeMap<_, _> = graph
            .node_indices()
            .map(|i| (i, new_graph.add_node(NWrap::from(graph[i].clone()))))
            .collect();
        for edge in graph.edge_references() {
            let (a, b) = (node_map[&edge.source()], node_map[&edge.target()]);
            new_graph.add_edge(a, b, EWrap::from(edge.weight().clone()));
        }
        new_graph
    }
}
