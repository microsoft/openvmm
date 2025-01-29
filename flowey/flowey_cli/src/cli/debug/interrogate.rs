// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::cli::FlowBackendCli;
use flowey_core::node::steps::rust::RustRuntimeServices;
use flowey_core::node::user_facing::ClaimedGhParam;
use flowey_core::node::user_facing::GhPermission;
use flowey_core::node::user_facing::GhPermissionValue;
use flowey_core::node::FlowArch;
use flowey_core::node::FlowBackend;
use flowey_core::node::FlowPlatform;
use flowey_core::node::GhVarState;
use flowey_core::node::NodeHandle;
use flowey_core::pipeline::HostExt;
use flowey_core::pipeline::PipelineBackendHint;
use std::collections::BTreeMap;

/// (debug) get info about a specific node.
///
/// Information includes:
/// - supported backends
/// - supported requests
/// - dependencies
/// - inline steps (with exec-snippet indices)*
///
/// *inline-steps will only be listed if they are enabled via the particular
/// combination of specified {backend x requests}. For a complete picture
/// possible dependencies and available steps, you must inspect the Node's
/// code and documentation directly.
#[derive(clap::Args)]
pub struct Interrogate {
    /// Node to interrogate
    node_handle: String,

    /// Flow backend to interrogate with
    flow_backend: FlowBackendCli,

    /// Apply a request onto the node (as JSON)
    #[clap(long)]
    req: Vec<String>,
}

impl Interrogate {
    pub fn run(self) -> anyhow::Result<()> {
        let Self {
            node_handle,
            flow_backend,
            req,
        } = self;

        let raw_json_reqs: Vec<Box<[u8]>> = req
            .into_iter()
            .map(|v| v.as_bytes().to_vec().into())
            .collect();

        let Some(node_handle) = NodeHandle::try_from_modpath(&node_handle) else {
            anyhow::bail!("could not find node with that name");
        };

        let mut node = node_handle.new_erased_node();

        let mut dep_registration_backend = InterrogateDepRegistrationBackend;
        let mut dep_registration = flowey_core::node::new_import_ctx(&mut dep_registration_backend);

        let mut ctx_backend = InterrogateCtx::new(flow_backend.into(), node_handle);

        println!(
            "# interrogating with {}",
            match flow_backend {
                FlowBackendCli::Ado => "ado",
                FlowBackendCli::Local => "local",
                FlowBackendCli::Github => "github",
            }
        );

        node.imports(&mut dep_registration);

        let mut ctx = flowey_core::node::new_node_ctx(&mut ctx_backend);
        node.emit(raw_json_reqs.clone(), &mut ctx)?;

        Ok(())
    }
}

struct InterrogateDepRegistrationBackend;

impl flowey_core::node::ImportCtxBackend for InterrogateDepRegistrationBackend {
    fn on_possible_dep(&mut self, node_handle: NodeHandle) {
        println!("[dep?] {}", node_handle.modpath())
    }
}

struct InterrogateCtx {
    flow_backend: FlowBackend,
    current_node: NodeHandle,
    idx_tracker: usize,
    var_tracker: usize,
}

impl InterrogateCtx {
    fn new(flow_backend: FlowBackend, current_node: NodeHandle) -> Self {
        Self {
            flow_backend,
            current_node,
            idx_tracker: 0,
            var_tracker: 0,
        }
    }
}

impl flowey_core::node::NodeCtxBackend for InterrogateCtx {
    fn on_emit_rust_step(
        &mut self,
        label: &str,
        _code: Box<
            dyn for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()> + 'static,
        >,
    ) {
        println!("[step][rust][{}] # {}", self.idx_tracker, label);
        self.idx_tracker += 1;
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
        _condvar: Option<String>,
    ) {
        println!(
            "[step][yaml]    # {}{}",
            if code.is_some() {
                "(+inline script) "
            } else {
                ""
            },
            label
        );
        let mut fresh_ado_var = || "<dummy>".into();
        let mut access = flowey_core::node::steps::ado::new_ado_step_services(&mut fresh_ado_var);
        let raw_snippet = yaml_snippet(&mut access);

        let snippet: Result<serde_yaml::Value, _> = serde_yaml::from_str(&raw_snippet);
        match snippet {
            Ok(snippet) => print!("{}", serde_yaml::to_string(&snippet).unwrap()),
            Err(e) => {
                log::error!("invalid snippet: {}", e);
                println!(">>>");
                println!("{}", raw_snippet);
                println!("<<<");
            }
        };

        self.idx_tracker += 1;
    }

    fn on_emit_gh_step(
        &mut self,

        label: &str,
        _uses: &str,
        _with: BTreeMap<String, ClaimedGhParam>,
        _condvar: Option<String>,
        _outputs: BTreeMap<String, Vec<GhVarState>>,
        _permissions: BTreeMap<GhPermission, GhPermissionValue>,
        _gh_to_rust: Vec<GhVarState>,
        _rust_to_gh: Vec<GhVarState>,
    ) {
        println!("[step][yaml]    # {}", label);
        self.idx_tracker += 1;
    }

    fn on_emit_side_effect_step(&mut self) {
        println!("[step][anchor]");
    }

    fn backend(&mut self) -> FlowBackend {
        self.flow_backend
    }

    fn platform(&mut self) -> FlowPlatform {
        FlowPlatform::host(PipelineBackendHint::Local)
    }

    fn arch(&mut self) -> FlowArch {
        // xtask-fmt allow-target-arch oneoff-flowey
        if cfg!(target_arch = "x86_64") {
            FlowArch::X86_64
        // xtask-fmt allow-target-arch oneoff-flowey
        } else if cfg!(target_arch = "aarch64") {
            FlowArch::Aarch64
        } else {
            unreachable!("flowey only runs on X86_64 or Aarch64 at the moment")
        }
    }

    fn on_request(&mut self, node_handle: NodeHandle, req: anyhow::Result<Box<[u8]>>) {
        match req {
            Ok(data) => {
                let data = match String::from_utf8(data.into()) {
                    Ok(data) => data,
                    Err(e) => e
                        .into_bytes()
                        .iter()
                        .map(|b| format!("(raw) {:02x}", b))
                        .collect::<Vec<_>>()
                        .join(""),
                };
                println!("[req] {} <-- {}", node_handle.modpath(), data)
            }
            Err(e) => {
                log::error!("error serializing inter-node request: {:#}", e)
            }
        }
    }

    fn on_new_var(&mut self) -> String {
        let v = self.var_tracker;
        self.var_tracker += 1;
        format!("<dummy>:{}", v)
    }

    fn on_claimed_runtime_var(&mut self, var: &str, is_read: bool) {
        println!(
            "[var][claim] {} {}",
            var,
            if is_read { "(read)" } else { "(write)" },
        )
    }

    fn current_node(&self) -> NodeHandle {
        self.current_node
    }

    fn persistent_dir_path_var(&mut self) -> Option<String> {
        Some("<dummy>".into())
    }

    fn on_unused_read_var(&mut self, _var: &str) {
        // not relevant
    }
}
