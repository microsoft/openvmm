// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared logic to set cfg_common params across various backends

use flowey::node::prelude::*;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::common::CommonArch;

#[derive(Clone, Default, clap::Args)]
#[clap(next_help_heading = "Local Only")]
pub struct LocalRunArgs {
    /// Emit verbose output when possible
    #[clap(long)]
    verbose: bool,

    /// Run builds with --locked
    #[clap(long)]
    pub locked: bool,

    /// Disable incremental compilation (sets CARGO_INCREMENTAL=0)
    #[clap(long)]
    pub no_incremental: bool,

    /// Automatically install all required dependencies
    #[clap(long)]
    auto_install_deps: bool,

    /// Don't prompt user when running certain interactive commands.
    #[clap(long)]
    non_interactive: bool,
}

pub type FulfillCommonRequestsParamsResolver =
    Box<dyn for<'a> Fn(&mut PipelineJobCtx<'a>) -> flowey_lib_hvlite::_jobs::cfg_common::Params>;

/// Whether a cloud pipeline should build with `-Dwarnings`.
///
/// Enabling this rewrites the tracked `.cargo/config.toml` in the checkout,
/// which leaves the working tree dirty. Any binary built afterwards is stamped
/// `.dirty` by `openvmm_build_info`, so pipelines that produce artifacts handed
/// to end users must leave this disabled and let the CI gates enforce warnings.
///
/// Has no effect on local runs, which never deny warnings.
#[derive(Clone, Copy)]
pub enum DenyWarnings {
    Enabled,
    Disabled,
}

fn get_params_local(
    local_run_args: Option<LocalRunArgs>,
) -> anyhow::Result<FulfillCommonRequestsParamsResolver> {
    Ok(Box::new(move |_ctx| {
        let LocalRunArgs {
            verbose,
            locked,
            no_incremental,
            auto_install_deps,
            non_interactive,
        } = local_run_args.clone().unwrap_or_default();

        flowey_lib_hvlite::_jobs::cfg_common::Params {
            local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                interactive: !non_interactive,
                auto_install: auto_install_deps,
                ignore_rust_version: true,
            }),
            verbose: ReadVar::from_static(verbose),
            locked,
            deny_warnings: false,
            no_incremental,
        }
    }))
}

fn get_params_cloud(
    pipeline: &mut Pipeline,
    deny_warnings: DenyWarnings,
) -> anyhow::Result<FulfillCommonRequestsParamsResolver> {
    let param_verbose = pipeline.new_parameter_bool(
        "verbose",
        "Run with verbose output",
        ParameterKind::Stable,
        Some(false),
    );

    Ok(Box::new(move |ctx: &mut PipelineJobCtx<'_>| {
        flowey_lib_hvlite::_jobs::cfg_common::Params {
            local_only: None,
            verbose: ctx.use_parameter(param_verbose.clone()),
            locked: true,
            deny_warnings: matches!(deny_warnings, DenyWarnings::Enabled),
            no_incremental: true,
        }
    }))
}

pub fn get_cfg_common_params(
    pipeline: &mut Pipeline,
    backend_hint: PipelineBackendHint,
    local_run_args: Option<LocalRunArgs>,
    deny_warnings: DenyWarnings,
) -> anyhow::Result<FulfillCommonRequestsParamsResolver> {
    match backend_hint {
        PipelineBackendHint::Local => get_params_local(local_run_args),
        PipelineBackendHint::Ado | PipelineBackendHint::Github => {
            if local_run_args.is_some() {
                anyhow::bail!("cannot set local only params when emitting as non-local pipeline")
            }
            get_params_cloud(pipeline, deny_warnings)
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, PartialEq)]
pub enum CommonArchCli {
    X86_64,
    Aarch64,
}

impl From<CommonArchCli> for CommonArch {
    fn from(value: CommonArchCli) -> Self {
        match value {
            CommonArchCli::X86_64 => CommonArch::X86_64,
            CommonArchCli::Aarch64 => CommonArch::Aarch64,
        }
    }
}

impl TryFrom<FlowArch> for CommonArchCli {
    type Error = anyhow::Error;

    fn try_from(arch: FlowArch) -> anyhow::Result<Self> {
        Ok(match arch {
            FlowArch::X86_64 => CommonArchCli::X86_64,
            FlowArch::Aarch64 => CommonArchCli::Aarch64,
            arch => anyhow::bail!("unsupported arch {arch}"),
        })
    }
}
