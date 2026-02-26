// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::cli::FlowBackendCli;
use anyhow::Context;
use flowey_core::node::FlowArch;
use flowey_core::node::FlowBackend;
use flowey_core::node::FlowPlatform;
use flowey_core::node::GhOutput;
use flowey_core::node::GhToRust;
use flowey_core::node::NodeHandle;
use flowey_core::node::RustToGh;
use flowey_core::node::steps::rust::RustRuntimeServices;
use flowey_core::node::user_facing::ClaimedGhParam;
use flowey_core::node::user_facing::GhPermission;
use flowey_core::node::user_facing::GhPermissionValue;
use flowey_core::pipeline::HostExt;
use flowey_core::pipeline::PipelineBackendHint;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub fn construct_exec_snippet_cli(
    flowey_bin: &str,
    node_modpath: &str,
    snippet_idx: usize,
    job_idx: usize,
) -> String {
    format!(r#"{flowey_bin} e {job_idx} {node_modpath} {snippet_idx}"#)
}

/// (internal) execute an inline code snippet from the given node.
#[derive(clap::Args)]
pub struct ExecSnippet {
    /// Job idx to query `pipeline_static_db` with
    pub(crate) job_idx: usize,

    node_modpath_and_snippet_idx: Vec<String>,

    /// (debug) If true, the snippet will not actually be run
    #[clap(long)]
    dry_run: bool,
}

pub const VAR_DB_SEEDVAR_FLOWEY_WORKING_DIR: &str = "_internal_WORKING_DIR";
pub const VAR_DB_SEEDVAR_FLOWEY_PERSISTENT_STORAGE_DIR: &str = "_internal_PERSISTENT_STORAGE_DIR";

impl ExecSnippet {
    pub fn run(self) -> anyhow::Result<()> {
        let Self {
            node_modpath_and_snippet_idx,
            job_idx,
            dry_run,
        } = self;

        let flow_platform = FlowPlatform::host(PipelineBackendHint::Local);
        let flow_arch = FlowArch::host(PipelineBackendHint::Local);

        let mut runtime_var_db = super::var_db::open_var_db(job_idx)?;

        let working_dir: PathBuf = {
            let Some((working_dir, _)) =
                runtime_var_db.try_get_var(VAR_DB_SEEDVAR_FLOWEY_WORKING_DIR)
            else {
                anyhow::bail!("var db was not seeded with {VAR_DB_SEEDVAR_FLOWEY_WORKING_DIR}");
            };
            serde_json::from_slice::<String>(&working_dir)
                .context(format!(
                    "found {VAR_DB_SEEDVAR_FLOWEY_WORKING_DIR} in db, but it wasn't a json string!"
                ))?
                .into()
        };

        let FloweyPipelineStaticDb {
            flow_backend,
            var_db_backend_kind: _,
            job_reqs,
            job_command_wrappers,
        } = {
            let current_exe = std::env::current_exe()
                .context("failed to get path to current flowey executable")?;
            let pipeline_static_db =
                fs_err::File::open(current_exe.with_file_name("pipeline.json"))?;
            serde_json::from_reader(pipeline_static_db)?
        };

        let command_wrapper = job_command_wrappers.get(&job_idx).cloned();

        for [node_modpath, snippet_idx] in node_modpath_and_snippet_idx
            .chunks_exact(2)
            .map(|x| -> [String; 2] { x.to_vec().try_into().unwrap() })
        {
            let snippet_idx = snippet_idx.parse::<usize>().unwrap();

            let raw_json_reqs: Vec<Box<[u8]>> = job_reqs
                .get(&job_idx)
                .context("invalid job_idx")?
                .get(&node_modpath)
                .context("pipeline db did not include data for specified node")?
                .iter()
                .map(|v| v.0.clone())
                .collect::<Vec<_>>();

            let Some(node_handle) = NodeHandle::try_from_modpath(&node_modpath) else {
                anyhow::bail!("could not find node with that name")
            };

            let mut node = node_handle.new_erased_node();

            // each snippet gets its own isolated working dir
            {
                let snippet_working_dir = working_dir.join(format!(
                    "{}_{}",
                    node_handle.modpath().replace("::", "__"),
                    snippet_idx
                ));
                if !snippet_working_dir.exists() {
                    fs_err::create_dir_all(&snippet_working_dir)?;
                }
                log::trace!(
                    "Setting current working directory from {:?} to {:?}",
                    std::env::current_dir()?,
                    snippet_working_dir
                );
                std::env::set_current_dir(snippet_working_dir)?;
            }

            // not all backends support a persistent storage dir, therefore it is optional
            let persistent_storage_dir_var = runtime_var_db
                .try_get_var(VAR_DB_SEEDVAR_FLOWEY_PERSISTENT_STORAGE_DIR)
                .is_some()
                .then_some(VAR_DB_SEEDVAR_FLOWEY_PERSISTENT_STORAGE_DIR.to_owned());

            let mut rust_runtime_services =
                flowey_core::node::steps::rust::new_rust_runtime_services(
                    &mut runtime_var_db,
                    flow_backend.into(),
                    flow_platform,
                    flow_arch,
                )?;

            if let Some(ref wrapper) = command_wrapper {
                rust_runtime_services.sh.set_wrapper(Some(wrapper.clone()));
            }

            let mut ctx_backend = ExecSnippetCtx::new(
                flow_backend.into(),
                flow_platform,
                flow_arch,
                node_handle,
                snippet_idx,
                dry_run,
                persistent_storage_dir_var,
                &mut rust_runtime_services,
            );

            let mut ctx = flowey_core::node::new_node_ctx(&mut ctx_backend);
            node.emit(raw_json_reqs.clone(), &mut ctx)?;

            match ctx_backend.into_result() {
                Some(res) => res?,
                None => {
                    if dry_run {
                        // all good, expected
                    } else {
                        anyhow::bail!("snippet wasn't run (invalid index)")
                    }
                }
            }
        }

        // Leave the last snippet's working dir so it can be deleted by later steps
        std::env::set_current_dir(working_dir)?;

        Ok(())
    }
}

pub struct ExecSnippetCtx<'a, 'b> {
    flow_backend: FlowBackend,
    flow_platform: FlowPlatform,
    flow_arch: FlowArch,
    node_handle: NodeHandle,
    rust_runtime_services: &'a mut RustRuntimeServices<'b>,
    idx_tracker: usize,
    var_tracker: usize,
    target_idx: usize,
    dry_run: bool,
    persistent_storage_dir_var: Option<String>,
    result: Option<anyhow::Result<()>>,
}

impl<'a, 'b> ExecSnippetCtx<'a, 'b> {
    pub fn new(
        flow_backend: FlowBackend,
        flow_platform: FlowPlatform,
        flow_arch: FlowArch,
        node_handle: NodeHandle,
        target_idx: usize,
        dry_run: bool,
        persistent_storage_dir_var: Option<String>,
        rust_runtime_services: &'a mut RustRuntimeServices<'b>,
    ) -> Self {
        Self {
            flow_backend,
            flow_platform,
            flow_arch,
            node_handle,
            rust_runtime_services,
            var_tracker: 0,
            idx_tracker: 0,
            target_idx,
            dry_run,
            persistent_storage_dir_var,
            result: None,
        }
    }

    pub fn into_result(self) -> Option<anyhow::Result<()>> {
        self.result
    }
}

impl flowey_core::node::NodeCtxBackend for ExecSnippetCtx<'_, '_> {
    fn on_request(&mut self, _node_handle: NodeHandle, _req: anyhow::Result<Box<[u8]>>) {
        // nothing to do - filing requests only matters pre-exec
    }

    fn on_new_var(&mut self) -> String {
        let v = self.var_tracker;
        self.var_tracker += 1;
        format!("{}:{}", self.node_handle.modpath(), v)
    }

    fn on_claimed_runtime_var(&mut self, _var: &str, _is_read: bool) {
        // nothing to do - variable claims only matter pre-exec
    }

    fn on_emit_rust_step(
        &mut self,
        label: &str,
        _can_merge: bool,
        code: Box<
            dyn for<'a> FnOnce(&'a mut RustRuntimeServices<'_>) -> anyhow::Result<()> + 'static,
        >,
    ) {
        if self.idx_tracker == self.target_idx {
            let label = if !label.is_empty() {
                label
            } else {
                "<unnamed snippet>"
            };

            self.result = Some(run_code(
                self.flow_backend,
                format!("{} ({})", label, self.node_handle.modpath()),
                self.dry_run,
                || code(self.rust_runtime_services),
            ));
        }
        self.idx_tracker += 1;
    }

    fn on_emit_ado_step(
        &mut self,
        label: &str,
        _yaml_snippet: Box<
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
        // don't need to care about condvar, since we wouldn't have been called
        // if the YAML resolved the condvar to false.
        if self.idx_tracker == self.target_idx {
            if let Some(code) = code {
                self.result = Some(run_code(
                    self.flow_backend,
                    format!(
                        "(inline snippet) {} ({})",
                        label,
                        self.node_handle.modpath()
                    ),
                    self.dry_run,
                    || code(self.rust_runtime_services),
                ));
            }
        }

        self.idx_tracker += 1;
    }

    fn on_emit_gh_step(
        &mut self,
        _label: &str,
        _uses: &str,
        _with: BTreeMap<String, ClaimedGhParam>,
        _condvar: Option<String>,
        _outputs: BTreeMap<String, Vec<GhOutput>>,
        _permissions: BTreeMap<GhPermission, GhPermissionValue>,
        _gh_to_rust: Vec<GhToRust>,
        _rust_to_gh: Vec<RustToGh>,
    ) {
        self.idx_tracker += 1;
    }

    fn on_emit_side_effect_step(&mut self) {
        // not executable, we simply skip
    }

    fn backend(&mut self) -> FlowBackend {
        self.flow_backend
    }

    fn platform(&mut self) -> FlowPlatform {
        self.flow_platform
    }

    fn arch(&mut self) -> FlowArch {
        self.flow_arch
    }

    fn current_node(&self) -> NodeHandle {
        self.node_handle
    }

    fn persistent_dir_path_var(&mut self) -> Option<String> {
        self.persistent_storage_dir_var.clone()
    }

    fn on_unused_read_var(&mut self, _var: &str) {
        // not relevant at runtime
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) enum VarDbBackendKind {
    Json,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct FloweyPipelineStaticDb {
    pub flow_backend: FlowBackendCli,
    pub var_db_backend_kind: VarDbBackendKind,
    pub job_reqs: BTreeMap<usize, BTreeMap<String, Vec<SerializedRequest>>>,
    pub job_command_wrappers: BTreeMap<usize, flowey_core::shell::CommandWrapperKind>,
}

// encode requests as JSON stored in a JSON string (to make human inspection
// easier).
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct SerializedRequest(#[serde(with = "serialized_request")] pub Box<[u8]>);

pub(crate) mod serialized_request {
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;

    #[expect(clippy::borrowed_box, reason = "required by serde")]
    pub fn serialize<S: Serializer>(v: &Box<[u8]>, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(
            &serde_json::to_string(&serde_json::from_slice::<serde_json::Value>(v).unwrap())
                .unwrap(),
        )
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Box<[u8]>, D::Error> {
        let s: String = Deserialize::deserialize(d)?;
        Ok(
            serde_json::to_vec(&serde_json::from_str::<serde_json::Value>(&s).unwrap())
                .unwrap()
                .into(),
        )
    }
}

fn run_code(
    flow_backend: FlowBackend,
    label: impl std::fmt::Display,
    dry_run: bool,
    code: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if matches!(flow_backend, FlowBackend::Ado) {
        println!("##[group]=== {} ===", label)
    } else {
        // green color
        log::info!("\x1B[0;32m=== {} ===\x1B[0m", label);
    }

    let result = if dry_run {
        log::info!("...but not actually, because of --dry-run");
        Ok(())
    } else {
        code()
    };

    // green color
    log::info!("\x1B[0;32m=== done! ===\x1B[0m");

    if matches!(flow_backend, FlowBackend::Ado) {
        println!("##[endgroup]")
    } else {
        log::info!(""); // log a newline, for the pretty
    }

    result
}
