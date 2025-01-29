// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use flowey_core::node::read_var_internals;
use flowey_core::node::steps::rust::RustRuntimeServices;
use flowey_core::node::user_facing::ClaimedGhParam;
use flowey_core::node::user_facing::GhPermission;
use flowey_core::node::user_facing::GhPermissionValue;
use flowey_core::node::FlowArch;
use flowey_core::node::FlowBackend;
use flowey_core::node::FlowPlatform;
use flowey_core::node::GhVarState;
use flowey_core::node::NodeHandle;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Clone)] // for viz
pub(crate) struct OutputGraphEntry {
    pub node_handle: NodeHandle,
    pub step: Step,
}

#[derive(Debug, Clone)]
pub(crate) enum DepKind {
    RuntimeVar,
    PostJob,
    PostJobInjected,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub(crate) struct StepId {
    pub node: NodeHandle,
    pub step_idx: usize,
}

/// A error that denotes the presence of unreachable nodes in the returned
/// graph.
///
/// Needs to be returned as part of the Ok(()) response to allow debugging with
/// --viz-mode.
pub(crate) struct FoundUnreachableNodes;

pub(crate) fn stage1_dag(
    backend: FlowBackend,
    platform: FlowPlatform,
    arch: FlowArch,
    mut resolved_patches: flowey_core::patch::ResolvedPatches,
    seed_nodes: BTreeMap<NodeHandle, (bool, Vec<Box<[u8]>>)>,
    external_read_vars: BTreeSet<String>,
    persistent_dir_path_var: Option<String>,
) -> Result<
    (
        petgraph::Graph<(StepId, Option<OutputGraphEntry>), DepKind>,
        BTreeMap<NodeHandle, Vec<Box<[u8]>>>,
        Option<FoundUnreachableNodes>,
    ),
    anyhow::Error,
> {
    // this output_graph and request_db are the two key items we'll be be
    // populating in the stage1 dag.

    // each node output_graph corresponds to a step to run, with the edges
    // corresponding to runtime dependencies between those steps.
    let mut output_graph = petgraph::Graph::<(StepId, Option<OutputGraphEntry>), DepKind>::new();
    // the request_db encodes the requests that each node were run with on this
    // particular pipeline. On "compiled" backends, it gets serialized as part
    // of a static "pipeline database" file that is referenced at runtime to
    // correctly re-run rust-based steps encoded within flowey.
    let mut request_db: BTreeMap<NodeHandle, Vec<Box<[u8]>>> = BTreeMap::new();

    let mut step_to_output_graph = BTreeMap::<StepId, petgraph::prelude::NodeIndex>::new();
    let mut get_output_node_idx =
        |g: &mut petgraph::Graph<(StepId, Option<OutputGraphEntry>), DepKind>, step| {
            // don't double-insert nodes
            match step_to_output_graph.get(&step) {
                Some(idx) => *idx,
                None => {
                    let idx = g.add_node((step, None));
                    let existing = step_to_output_graph.insert(step, idx);
                    assert!(existing.is_none());
                    idx
                }
            }
        };

    // the very first node we throw into the graph is the so called "root anchor
    // node", which is the node that all pipeline-level `done` side-effects hang
    // off of.
    let (root_anchor_stepid, root_anchor_idx) = {
        let node_handle = NodeHandle::dummy();
        let root_anchor_stepid = StepId {
            node: node_handle,
            step_idx: 0,
        };
        let root_anchor_idx = get_output_node_idx(&mut output_graph, root_anchor_stepid);
        assert!(output_graph[root_anchor_idx].1.is_none());
        output_graph[root_anchor_idx].1 = Some(OutputGraphEntry {
            node_handle,
            step: Step::Anchor {
                label: "pipeline_root",
            },
        });

        (root_anchor_stepid, root_anchor_idx)
    };

    // we also throw in a node which we can hang `claim_unused` ReadVars off of,
    // so that they they can be culled during final graph resolution
    let (unused_readvar_stepid, unused_readvar_idx) = {
        let node_handle = NodeHandle::dummy();
        let unused_readvar_stepid = StepId {
            node: node_handle,
            step_idx: 1,
        };
        let unused_readvar_idx = get_output_node_idx(&mut output_graph, unused_readvar_stepid);
        assert!(output_graph[unused_readvar_idx].1.is_none());
        output_graph[unused_readvar_idx].1 = Some(OutputGraphEntry {
            node_handle,
            step: Step::Anchor {
                label: "unused_readvar",
            },
        });

        (unused_readvar_stepid, unused_readvar_idx)
    };

    let start_nodes = seed_nodes
        .iter()
        .filter_map(|(node, (root, _reqs))| root.then_some(*node))
        .collect::<BTreeSet<_>>();

    let external_read_vars = {
        let mut external_read_vars = external_read_vars;
        if let Some(var) = &persistent_dir_path_var {
            external_read_vars.insert(var.clone());
        }
        external_read_vars
    };

    let patch_node = |node_handle| {
        if let Some(swap_node_handle) = resolved_patches.swap.get(&node_handle) {
            *swap_node_handle
        } else {
            node_handle
        }
    };

    let (_, stage0_order) = {
        match super::stage0_dag::stage0_dag_and_toposort(
            &start_nodes.iter().copied().collect::<Vec<_>>(),
            &resolved_patches,
        ) {
            Ok(v) => v,
            Err(e) => match e {
                super::stage0_dag::Stage0DagError::UnsupportedBackend(node_handle) => {
                    anyhow::bail!(
                        "{} doesn't support the specified backend",
                        node_handle.modpath()
                    );
                }
                super::stage0_dag::Stage0DagError::Cycle => {
                    anyhow::bail!("detected cycle!")
                }
            },
        }
    };

    // now, we set up some important bookkeeping vars that will be populated
    // while traversing down the stage0 DAG

    // maintain a list of pending reqs for nodes further down the order.
    //
    // the meaning of the `bool` changes in this context, from being a "root"
    // node, to instead selecting whether or not the node has been "activated"
    // by some node.
    //
    // the root nodes can be thought of as nodes that were activated by an
    // external entity (as opposed to some other node).
    let mut outstanding_reqs: BTreeMap<NodeHandle, (/*activated*/ bool, Vec<Box<[u8]>>)> =
        seed_nodes;

    // maintain an outstanding queue of runtime variable connections that need
    // to be fulfilled
    #[derive(Default)]
    struct OutstandingVarEntry {
        read_by: Vec<StepId>,
        written_by: Option<StepId>,
    }
    let mut outstanding_vars = BTreeMap::<String, OutstandingVarEntry>::new();

    let mut yaml_var_ordinal = 0;
    let mut encountered_error = false;
    for node_handle in stage0_order {
        log::debug!("visiting {}", node_handle.modpath());

        let mut requests = match outstanding_reqs.remove(&node_handle) {
            Some((true, requests)) => requests,
            // this node was never activated, so we skip it.
            Some((false, _)) | None => continue,
        };

        // HACK: flowey still has some issues with inconsistent traversal order
        // on windows / linux, so to avoid having frontends emit slightly
        // different outputs between platforms, apply this "band-aid" sort to
        // ensure a consistent request iteration order in each node.
        requests.sort();
        let requests = requests;

        for req in &requests {
            log::trace!("using req: {}", String::from_utf8_lossy(req));
        }

        {
            let existing = request_db.insert(node_handle, requests.clone());
            assert!(existing.is_none());
        };

        let (imports, events) = {
            let mut node = node_handle.new_erased_node();

            let mut dep_registration_backend =
                super::stage0_dag::CollectDepRegistrationBackend::new(&patch_node);
            let mut dep_registration =
                flowey_core::node::new_import_ctx(&mut dep_registration_backend);

            let mut ctx_backend = EmitFlowCtx::new(
                node_handle,
                backend,
                platform,
                arch,
                persistent_dir_path_var.clone(),
                &patch_node,
                &mut yaml_var_ordinal,
            );
            let mut ctx = flowey_core::node::new_node_ctx(&mut ctx_backend);

            node.imports(&mut dep_registration);
            if let Err(e) = node.emit(requests.clone(), &mut ctx) {
                // don't want to immediately bail, since some errors could be a
                // by-product of other errors (e.g: forgetting to include a
                // imports can result in an incorrect visit order,
                // resulting in spurious missing request errors).
                log::error!("error while processing {}: {}", node_handle.modpath(), e);
                encountered_error = true;
                continue;
            }

            let mut imports = dep_registration_backend.into_deps();
            let mut events = ctx_backend.into_events();

            // include injected side effects
            if let Some(deps) = resolved_patches.inject_side_effect.remove(&node_handle) {
                for (to, var, req) in deps {
                    imports.insert(to);

                    let mut new_events = vec![EmitEvent::Request {
                        dep_node: to,
                        req: Ok(req),
                    }];
                    let mut found_write_var = false;
                    for event in std::mem::take(&mut events) {
                        if matches!(&event, EmitEvent::ClaimWriteVar { .. }) {
                            found_write_var = true;
                            new_events.push(EmitEvent::ClaimReadVar { var: var.clone() });
                        }
                        new_events.push(event);
                    }

                    if !found_write_var {
                        anyhow::bail!(
                            "cannot inject side effect into {:?}: node did not claim any WriteVars",
                            node_handle
                        );
                    }

                    events = new_events;
                }
            }

            imports.extend(
                resolved_patches
                    .inject_side_effect
                    .get(&node_handle)
                    .iter()
                    .flat_map(|x| x.iter().map(|x| x.0)),
            );

            (imports, events)
        };

        let mut steps = Vec::new();

        for event in events {
            match event {
                EmitEvent::ClaimReadVar { var } => {
                    outstanding_vars
                        .entry(var)
                        .or_default()
                        .read_by
                        .push(StepId {
                            node: node_handle,
                            // comes before the `EmitStep` it corresponds to.
                            step_idx: steps.len(),
                        });
                }
                EmitEvent::ClaimUnused { var } => {
                    outstanding_vars
                        .entry(var)
                        .or_default()
                        .read_by
                        .push(unused_readvar_stepid);
                }
                EmitEvent::ClaimWriteVar { var } => {
                    let written_by =
                        &mut outstanding_vars.entry(var.clone()).or_default().written_by;

                    assert!(
                        written_by.is_none(),
                        "{} write conflict: {} vs {}",
                        var,
                        node_handle.modpath(),
                        written_by.unwrap().node.modpath()
                    );
                    *written_by = Some(StepId {
                        node: node_handle,
                        // comes before the `EmitStep` it corresponds to.
                        step_idx: steps.len(),
                    })
                }
                EmitEvent::EmitStep { step } => steps.push(step),
                EmitEvent::Request { dep_node, req } => {
                    // while we're here... double check that the dep was reported in
                    // the imports list
                    if !imports.contains(&dep_node) {
                        anyhow::bail!(
                            "{} took a dep on {} without including it in `imports`",
                            node_handle.modpath(),
                            dep_node.modpath()
                        );
                    }

                    // add the request to the list of outstanding reqs, and make
                    // sure to "activate" the dep node
                    let (active, reqs) = outstanding_reqs.entry(dep_node).or_default();
                    *active = true;
                    reqs.push(req.context(format!(
                        "failed to serialize request to {} (via {})",
                        dep_node.modpath(),
                        node_handle.modpath()
                    ))?);
                }
            }
        }

        for (step_idx, step) in steps.into_iter().enumerate() {
            let current_output_node_idx = get_output_node_idx(
                &mut output_graph,
                StepId {
                    node: node_handle,
                    step_idx,
                },
            );
            assert!(output_graph[current_output_node_idx].1.is_none());
            output_graph[current_output_node_idx].1 = Some(OutputGraphEntry { node_handle, step });
        }
    }

    if encountered_error {
        anyhow::bail!("encountered one or more errors");
    }

    // post-job (owner step, post-job step)
    //
    // used to determine which post-job steps are reachable in the final graph
    let mut post_job_steps: BTreeMap<StepId, Vec<StepId>> = BTreeMap::new();

    // handle resolving runtime variable dependencies
    for (
        var,
        OutstandingVarEntry {
            read_by,
            written_by,
        },
    ) in outstanding_vars
    {
        let (skip_written_by, skip_read_by) = {
            if external_read_vars.contains(&var) {
                (true, false)
            } else {
                (false, false)
            }
        };

        // FUTURE: improve error handling!
        if !skip_written_by {
            assert!(written_by.is_some(), "var is never written: {var}");
        }

        // vars that begin with 'start' correspond to the done vars handed out
        // by top-level deps in each pipeline.
        let read_by = if var.starts_with("start") {
            vec![root_anchor_stepid]
        } else {
            read_by
        };

        if read_by.is_empty() && !var.starts_with("auto_se") {
            log::warn!("var is never read: {var}")
        }

        if var.starts_with("post_job") {
            // inject extra dep between post_job var readers, and the root anchor node
            for read_node_handle in &read_by {
                output_graph.add_edge(
                    *step_to_output_graph
                        .get(read_node_handle)
                        .unwrap_or_else(|| panic!("{var} {:?}", read_node_handle)),
                    root_anchor_idx,
                    DepKind::PostJobInjected,
                );
            }

            // jot down what job step owns the corresponding post-job step(s)
            post_job_steps
                .entry(written_by.unwrap())
                .or_default()
                .extend(read_by.clone());
        }

        // apply intra-graph connection
        if !(skip_read_by || skip_written_by) {
            let write_node_handle = written_by.unwrap();
            for read_node_handle in read_by {
                output_graph.add_edge(
                    *step_to_output_graph
                        .get(&read_node_handle)
                        .unwrap_or_else(|| panic!("{var} {:?}", read_node_handle)),
                    *step_to_output_graph
                        .get(&write_node_handle)
                        .unwrap_or_else(|| panic!("{var} {:?}", write_node_handle)),
                    if var.starts_with("post_job") {
                        DepKind::PostJob
                    } else {
                        DepKind::RuntimeVar
                    },
                );
            }
        }
    }

    // at this point, the graph should be fully resolved, and all that's left is
    // to do some final validation to check for buggy nodes that didn't
    // early-bail if they only received ReadVar requests, with no corresponding
    // WriteVar requests.
    //
    // this can happen in cases where a node simply emits a bunch of
    // "configuration" requests, setting bits of required data on various nodes
    // without actually requesting any output / side-effects from the node. e.g:
    // HvLite's `fulfill_common_requests` node is an example of this.

    // 1. determine which steps are reachable from the root anchor.
    let mut reachable_from_root_idxs = BTreeSet::new();
    {
        let mut dfs = petgraph::visit::Dfs::new(&output_graph, root_anchor_idx);
        while let Some(idx) = dfs.next(&output_graph) {
            let no_existing = reachable_from_root_idxs.insert(idx);
            assert!(no_existing);
        }
    }

    // 2. determine which reachable steps are relying on post-job steps to run,
    //    and mark post-job steps (and their deps) as required.
    let mut reachable_from_post_job_idxs = BTreeSet::new();
    {
        // FUTURE: this is _horribly_ inefficient, and should be improved to
        // support large dependency trees.
        for (owner_step, post_job_steps) in post_job_steps {
            if reachable_from_root_idxs.contains(step_to_output_graph.get(&owner_step).unwrap()) {
                for post_job_step in post_job_steps {
                    let mut dfs = petgraph::visit::Dfs::new(
                        &output_graph,
                        *step_to_output_graph.get(&post_job_step).unwrap(),
                    );
                    while let Some(idx) = dfs.next(&output_graph) {
                        reachable_from_post_job_idxs.insert(idx);
                    }
                }
            }
        }
    }

    // 3. avoid doing work for vars explicitly claimed as unused
    let doing_useless_work = {
        let mut to_delete = Vec::new();
        let mut to_visit = vec![unused_readvar_idx];
        while let Some(idx) = to_visit.pop() {
            if reachable_from_root_idxs.contains(&idx) {
                continue;
            }

            to_delete.push(idx);
            to_visit.extend(
                output_graph
                    .edges_directed(idx, petgraph::Direction::Outgoing)
                    .map(|e| petgraph::visit::EdgeRef::target(&e)),
            );
        }
        to_delete.sort();
        to_delete.dedup();
        to_delete
    };

    // 4. error out if there are unexpectedly unreachable nodes from the graph
    let found_unreachable_nodes = {
        // assume every idx is gonna go, and remove any nodes that will be
        // preserved
        let mut unreachable_idxs = output_graph.node_indices().collect::<BTreeSet<_>>();
        for idx in reachable_from_root_idxs
            .into_iter()
            .chain(reachable_from_post_job_idxs)
            .chain(doing_useless_work.clone())
        {
            unreachable_idxs.remove(&idx);
        }

        if !unreachable_idxs.is_empty() {
            for idx in unreachable_idxs {
                log::error!(
                    "found unreachable step in node {:?} - {}",
                    output_graph[idx].0.node,
                    output_graph[idx].1.as_ref().unwrap().step.label()
                );
            }
            log::error!("found buggy node that emitted unreachable steps! use `--viz-mode flow-dot` to debug");
            Some(FoundUnreachableNodes)
        } else {
            None
        }
    };

    // 5. cull nodes that should be deleted
    {
        // when removing a node from a petgraph, any nodes with an idx greater
        // than the node idx being removed become invalidated. As such, we do
        // the removal in reverse idx order.
        let mut remove_idxs = doing_useless_work;
        remove_idxs.sort();
        for idx in remove_idxs.into_iter().rev() {
            output_graph.remove_node(idx);
        }
    }

    Ok((output_graph, request_db, found_unreachable_nodes))
}

#[derive(Clone)] // for viz
pub(crate) enum Step {
    Anchor {
        label: &'static str,
    },
    Rust {
        idx: usize,
        label: String,
        // FIXME: this absolutely cursed type is only here due to that Clone
        // bound, which is itself only required due to the really shoddy code in
        // the petgraph viz backend.
        //
        // this should be easily fixed.
        code: Arc<
            Mutex<
                Option<
                    Box<
                        dyn for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()>
                            + 'static,
                    >,
                >,
            >,
        >,
    },
    AdoYaml {
        ado_to_rust: Vec<(String, String, bool)>,
        rust_to_ado: Vec<(String, String, bool)>,
        label: String,
        raw_yaml: String,
        condvar: Option<String>,
        // FIXME: see above
        code_idx: usize,
        code: Arc<
            Mutex<
                Option<
                    Box<
                        dyn for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()>
                            + 'static,
                    >,
                >,
            >,
        >,
    },
    GitHubYaml {
        gh_to_rust: Vec<GhVarState>,
        rust_to_gh: Vec<GhVarState>,
        label: String,
        step_id: String,
        uses: String,
        with: BTreeMap<String, String>,
        condvar: Option<String>,
        permissions: BTreeMap<GhPermission, GhPermissionValue>,
    },
}

impl Step {
    fn label(&self) -> &str {
        match self {
            Step::Anchor { label } => label,
            Step::Rust { label, .. } => label,
            Step::AdoYaml { label, .. } => label,
            Step::GitHubYaml { label, .. } => label,
        }
    }
}

enum EmitEvent {
    Request {
        dep_node: NodeHandle,
        req: anyhow::Result<Box<[u8]>>,
    },
    ClaimReadVar {
        var: String,
    },
    ClaimWriteVar {
        var: String,
    },
    EmitStep {
        step: Step,
    },
    ClaimUnused {
        var: String,
    },
}

struct EmitFlowCtx<'a> {
    current_node: NodeHandle,
    patch_node: &'a dyn Fn(NodeHandle) -> NodeHandle,
    backend: FlowBackend,
    platform: FlowPlatform,
    arch: FlowArch,
    step_idx_tracker: usize,
    var_tracker: usize,
    yaml_var_ordinal: &'a mut usize,
    persistent_dir_path_var: Option<String>,
    events: Vec<EmitEvent>,
}

impl<'a> EmitFlowCtx<'a> {
    fn new(
        current_node: NodeHandle,
        backend: FlowBackend,
        platform: FlowPlatform,
        arch: FlowArch,
        persistent_dir_path_var: Option<String>,
        patch_node: &'a dyn Fn(NodeHandle) -> NodeHandle,
        yaml_var_ordinal: &'a mut usize,
    ) -> Self {
        Self {
            current_node,
            patch_node,
            backend,
            platform,
            arch,
            step_idx_tracker: 0,
            var_tracker: 0,
            yaml_var_ordinal,
            persistent_dir_path_var,
            events: Vec::new(),
        }
    }

    fn into_events(self) -> Vec<EmitEvent> {
        self.events
    }
}

impl flowey_core::node::NodeCtxBackend for EmitFlowCtx<'_> {
    fn on_request(&mut self, node_handle: NodeHandle, req: anyhow::Result<Box<[u8]>>) {
        self.events.push(EmitEvent::Request {
            dep_node: (self.patch_node)(node_handle),
            req,
        });
    }

    fn on_new_var(&mut self) -> String {
        let v = self.var_tracker;
        self.var_tracker += 1;
        format!("{}:{}", self.current_node.modpath(), v)
    }

    fn on_unused_read_var(&mut self, var: &str) {
        self.events.push(EmitEvent::ClaimUnused { var: var.into() })
    }

    fn on_claimed_runtime_var(&mut self, var: &str, is_read: bool) {
        self.events.push(if is_read {
            EmitEvent::ClaimReadVar { var: var.into() }
        } else {
            EmitEvent::ClaimWriteVar { var: var.into() }
        })
    }

    fn on_emit_rust_step(
        &mut self,
        label: &str,
        code: Box<
            dyn for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()> + 'static,
        >,
    ) {
        self.events.push(EmitEvent::EmitStep {
            step: Step::Rust {
                idx: self.step_idx_tracker,
                label: label.into(),
                #[allow(clippy::arc_with_non_send_sync)]
                code: Arc::new(Mutex::new(Some(code))),
            },
        });
        self.step_idx_tracker += 1;
    }

    fn on_emit_ado_step(
        &mut self,
        label: &str,
        yaml_snippet: Box<
            dyn for<'a> FnOnce(
                &'a mut flowey_core::node::user_facing::AdoStepServices<'_>,
            ) -> String,
        >,
        code: Option<
            Box<
                dyn for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()> + 'static,
            >,
        >,
        condvar: Option<String>,
    ) {
        let mut fresh_yaml_var = || {
            *self.yaml_var_ordinal += 1;
            format!("floweyvar{}", self.yaml_var_ordinal)
        };
        let mut access = flowey_core::node::steps::ado::new_ado_step_services(&mut fresh_yaml_var);
        let raw_yaml = yaml_snippet(&mut access);
        let flowey_core::node::steps::ado::CompletedAdoStepServices {
            ado_to_rust,
            rust_to_ado,
        } = flowey_core::node::steps::ado::CompletedAdoStepServices::from_ado_step_services(access);

        if let Some(condvar) = &condvar {
            self.events.push(EmitEvent::ClaimReadVar {
                var: condvar.clone(),
            })
        }

        self.events.push(EmitEvent::EmitStep {
            step: Step::AdoYaml {
                label: label.into(),
                raw_yaml,
                code_idx: self.step_idx_tracker,
                #[allow(clippy::arc_with_non_send_sync)]
                code: Arc::new(Mutex::new(code)),
                ado_to_rust,
                rust_to_ado,
                condvar,
            },
        });
        self.step_idx_tracker += 1;
    }

    fn on_emit_gh_step(
        &mut self,
        label: &str,
        uses: &str,
        with: BTreeMap<String, ClaimedGhParam>,
        condvar: Option<String>,
        outputs: BTreeMap<String, Vec<GhVarState>>,
        permissions: BTreeMap<GhPermission, GhPermissionValue>,
        mut gh_to_rust: Vec<GhVarState>,
        mut rust_to_gh: Vec<GhVarState>,
    ) {
        let mut fresh_yaml_var = || {
            *self.yaml_var_ordinal += 1;
            format!("floweyvar{}", self.yaml_var_ordinal)
        };

        if let Some(condvar) = &condvar {
            self.events.push(EmitEvent::ClaimReadVar {
                var: condvar.clone(),
            })
        }

        let node_modpath = self.current_node.modpath().replace("::", "__");
        let step_id = format!("{node_modpath}__{}", self.step_idx_tracker);
        let with = with
            .into_iter()
            .map(|(k, v)| match v {
                ClaimedGhParam::Static(v) => (k, v),
                ClaimedGhParam::FloweyVar(v) => {
                    let (backing_var, is_secret) = read_var_internals(&v);
                    let backing_var = backing_var.unwrap();
                    let new_gh_var_name = fresh_yaml_var();
                    rust_to_gh.push(GhVarState {
                        backing_var: backing_var.clone(),
                        raw_name: Some(new_gh_var_name.clone()),
                        is_object: false,
                        is_secret,
                    });
                    (k, format!("${{{{ env.{} }}}}", new_gh_var_name))
                }
            })
            .collect();

        for (name, output_vars) in outputs {
            for output in output_vars {
                let gh_context_var_name = format!("steps.{step_id}.outputs.{name}");
                gh_to_rust.push(GhVarState {
                    backing_var: output.backing_var,
                    raw_name: Some(gh_context_var_name),
                    is_secret: output.is_secret,
                    is_object: output.is_object,
                });
            }
        }

        self.events.push(EmitEvent::EmitStep {
            step: Step::GitHubYaml {
                gh_to_rust,
                rust_to_gh,
                label: label.into(),
                step_id,
                uses: uses.into(),
                with,
                condvar,
                permissions,
            },
        });
        self.step_idx_tracker += 1;
    }

    fn on_emit_side_effect_step(&mut self) {
        self.events.push(EmitEvent::EmitStep {
            step: Step::Anchor {
                label: "side-effect-step",
            },
        });
    }

    fn backend(&mut self) -> FlowBackend {
        self.backend
    }

    fn platform(&mut self) -> FlowPlatform {
        self.platform
    }

    fn arch(&mut self) -> FlowArch {
        self.arch
    }

    fn current_node(&self) -> NodeHandle {
        self.current_node
    }

    fn persistent_dir_path_var(&mut self) -> Option<String> {
        self.persistent_dir_path_var.clone()
    }
}
