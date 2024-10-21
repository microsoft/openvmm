// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use flowey_core::node::NodeHandle;

#[derive(Clone, clap::ValueEnum)]
pub enum VizModeCli {
    Toposort,
    Dot,
}

/// (debug) dump stage0 DAG from a given node
///
/// The stage0 DAG only includes dependencies encoded via `imports`, and
/// does not take into consideration deps that are gated behind requests /
/// caused by runtime variables.
#[derive(clap::Args)]
pub struct DumpStage0Dag {
    /// Nodes to start the DAG from
    node_handles: Vec<String>,

    /// Incorporate a patch into the module graph.
    ///
    /// Use `list-patches` to get a list of available patches.
    #[clap(long)]
    with_patch: Vec<String>,

    /// Visualization mode
    #[clap(long)]
    viz_mode: VizModeCli,
}

impl DumpStage0Dag {
    pub fn run(self) -> anyhow::Result<()> {
        let Self {
            node_handles,
            with_patch,
            viz_mode,
        } = self;

        let mut start_node_handles = Vec::new();
        for s in node_handles {
            start_node_handles.push(
                NodeHandle::try_from_modpath(&s)
                    .context(format!("could not find node with name {s}"))?,
            )
        }

        let resolved_patches = {
            let mut patch_aggregator = flowey_core::patch::ResolvedPatches::build();
            for patchfn_name in with_patch {
                let Some(patchfn) =
                    flowey_core::patch::patchfn_by_modpath().get(patchfn_name.as_str())
                else {
                    anyhow::bail!("could not find patch with name '{patchfn_name}'. Was it registered with `flowey_core::node::register_patch!`?")
                };
                patch_aggregator.apply_patchfn(*patchfn);
            }
            patch_aggregator.finalize()
        };

        let (g, order) = {
            match crate::flow_resolver::stage0_dag::stage0_dag_and_toposort(
                &start_node_handles,
                &resolved_patches,
            ) {
                Ok(v) => v,
                Err(e) => match e {
                    crate::flow_resolver::stage0_dag::Stage0DagError::UnsupportedBackend(
                        node_handle,
                    ) => {
                        anyhow::bail!(
                            "{} doesn't support the specified backend",
                            node_handle.modpath()
                        );
                    }
                    crate::flow_resolver::stage0_dag::Stage0DagError::Cycle => {
                        anyhow::bail!("detected cycle!")
                    }
                },
            }
        };

        match viz_mode {
            VizModeCli::Toposort => {
                for node_handle in order {
                    println!("{}", node_handle.modpath())
                }
            }
            VizModeCli::Dot => {
                #[derive(Clone)]
                struct VizNode(NodeHandle);

                impl From<NodeHandle> for VizNode {
                    fn from(value: NodeHandle) -> Self {
                        Self(value)
                    }
                }

                impl std::fmt::Debug for VizNode {
                    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        write!(f, "{}", self.0.modpath())
                    }
                }

                println!(
                    "{:?}",
                    petgraph::dot::Dot::with_config(
                        &crate::pipeline_resolver::viz::petgraph_viz_helper::clone_graph_with_wrappers::<_, _, VizNode, ()>(&g),
                        &[petgraph::dot::Config::EdgeNoLabel]
                    )
                );
            }
        }

        Ok(())
    }
}
