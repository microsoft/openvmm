// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared logic to set cfg_common params across various backends

use flowey::node::prelude::*;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::run_cargo_build::common::CommonArch;

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
            deny_warnings: true,
            no_incremental: true,
        }
    }))
}

pub fn get_cfg_common_params(
    pipeline: &mut Pipeline,
    backend_hint: PipelineBackendHint,
    local_run_args: Option<LocalRunArgs>,
) -> anyhow::Result<FulfillCommonRequestsParamsResolver> {
    match backend_hint {
        PipelineBackendHint::Local => get_params_local(local_run_args),
        PipelineBackendHint::Ado | PipelineBackendHint::Github => {
            if local_run_args.is_some() {
                anyhow::bail!("cannot set local only params when emitting as non-local pipeline")
            }
            get_params_cloud(pipeline)
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
