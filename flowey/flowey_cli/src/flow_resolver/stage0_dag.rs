// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey_core::node::NodeHandle;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub enum Stage0DagError {
    UnsupportedBackend(NodeHandle),
    // FUTURE: include more context on the cycle
    Cycle,
}

pub(crate) fn stage0_dag_and_toposort(
    start_node_handles: &[NodeHandle],
    patches: &flowey_core::patch::ResolvedPatches,
) -> Result<(petgraph::Graph<NodeHandle, ()>, Vec<NodeHandle>), Stage0DagError> {
    let patch_node = |node_handle| {
        if let Some(swap_node_handle) = patches.swap.get(&node_handle) {
            *swap_node_handle
        } else {
            node_handle
        }
    };

    let get_node_deps = |node_handle: NodeHandle| {
        let mut dep_registration_backend = CollectDepRegistrationBackend::new(&patch_node);
        let mut dep_registration = flowey_core::node::new_import_ctx(&mut dep_registration_backend);
        let mut node = node_handle.new_erased_node();
        node.imports(&mut dep_registration);
        let mut deps = dep_registration_backend.into_deps();
        deps.extend(
            patches
                .inject_side_effect
                .get(&node_handle)
                .iter()
                .flat_map(|x| x.iter().map(|x| x.0)),
        );
        Ok(deps)
    };

    generic_stage0_dag_and_toposort(start_node_handles, get_node_deps, patch_node)
}

fn generic_stage0_dag_and_toposort(
    start_node_handles: &[NodeHandle],
    mut get_node_deps: impl FnMut(NodeHandle) -> Result<BTreeSet<NodeHandle>, ()>,
    patch_node: impl Fn(NodeHandle) -> NodeHandle,
) -> Result<(petgraph::Graph<NodeHandle, ()>, Vec<NodeHandle>), Stage0DagError> {
    let mut g = petgraph::Graph::<NodeHandle, ()>::new();

    let mut inserted_nodes: BTreeMap<NodeHandle, petgraph::prelude::NodeIndex> = BTreeMap::new();
    let mut visited_nodes = BTreeSet::new();
    let mut nodes_to_visit = BTreeSet::new();
    nodes_to_visit.extend(start_node_handles);

    while let Some(node_handle) = nodes_to_visit.pop_first() {
        let node_handle = patch_node(node_handle);

        let node_deps = get_node_deps(node_handle)
            .map_err(|_| Stage0DagError::UnsupportedBackend(node_handle))?;

        // update `g` with the new connections, inserting as-yet
        // unseen nodes into the graph as required.

        let mut get_node_idx = |g: &mut petgraph::Graph<_, _>, node_handle| {
            // don't double-insert nodes
            match inserted_nodes.get(&node_handle) {
                Some(idx) => *idx,
                None => {
                    let idx = g.add_node(node_handle);
                    let existing = inserted_nodes.insert(node_handle, idx);
                    assert!(existing.is_none());
                    idx
                }
            }
        };

        let current_node_idx = get_node_idx(&mut g, node_handle);
        for dep_node_handle in node_deps {
            let dep_node_idx = get_node_idx(&mut g, dep_node_handle);

            g.add_edge(current_node_idx, dep_node_idx, ());

            if !visited_nodes.contains(&dep_node_handle) {
                nodes_to_visit.insert(dep_node_handle);
            }
        }

        visited_nodes.insert(node_handle);
    }

    let node_order = petgraph::algo::toposort(&g, None)
        // FUTURE: add more context to the error when a cycle is detected
        .map_err(|_| Stage0DagError::Cycle)?
        .into_iter()
        .map(|idx| g[idx])
        .collect();

    Ok((g, node_order))
}

pub struct CollectDepRegistrationBackend<'a> {
    patch_node: &'a dyn Fn(NodeHandle) -> NodeHandle,
    deps: BTreeSet<NodeHandle>,
}

impl<'a> CollectDepRegistrationBackend<'a> {
    pub fn new(patch_node: &'a dyn Fn(NodeHandle) -> NodeHandle) -> Self {
        Self {
            patch_node,
            deps: BTreeSet::new(),
        }
    }

    pub fn into_deps(self) -> BTreeSet<NodeHandle> {
        self.deps
    }
}

impl flowey_core::node::ImportCtxBackend for CollectDepRegistrationBackend<'_> {
    fn on_possible_dep(&mut self, node_typeid: NodeHandle) {
        self.deps.insert((self.patch_node)(node_typeid));
    }
}
