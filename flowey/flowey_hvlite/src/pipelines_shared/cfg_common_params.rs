// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared logic to set cfg_common params across various backends

use flowey::node::prelude::*;
use flowey::pipeline::prelude::*;

#[derive(Clone, Default, clap::Args)]
#[clap(next_help_heading = "Local Only")]
pub struct LocalRunArgs {
    /// Emit verbose output when possible
    #[clap(long)]
    verbose: bool,

    /// Run builds with --locked
    #[clap(long)]
    locked: bool,

    /// Automatically install all required dependencies
    #[clap(long)]
    auto_install_deps: bool,

    /// Don't prompt user when running certain interactive commands.
    #[clap(long)]
    non_interactive: bool,

    /// (WSL2 only) Force the use of `mono` to download nuget packages.
    #[clap(long)]
    force_nuget_mono: bool,

    /// Claim that nuget is using an external auth mechanism.
    ///
    /// This will skip the check to make sure Azure Credential Provider is
    /// installed.
    #[clap(long)]
    external_nuget_auth: bool,
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
            auto_install_deps,
            non_interactive,
            force_nuget_mono,
            external_nuget_auth,
        } = local_run_args.clone().unwrap_or_default();

        flowey_lib_hvlite::_jobs::cfg_common::Params {
            local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                interactive: !non_interactive,
                auto_install: auto_install_deps,
                force_nuget_mono,
                external_nuget_auth,
                ignore_rust_version: true,
            }),
            verbose: ReadVar::from_static(verbose),
            locked,
            deny_warnings: false,
        }
    }))
}

fn get_params_cloud(
    pipeline: &mut Pipeline,
) -> anyhow::Result<FulfillCommonRequestsParamsResolver> {
    let param_verbose = pipeline.new_parameter_bool("Run with verbose output", Some(false));

    Ok(Box::new(move |ctx: &mut PipelineJobCtx<'_>| {
        flowey_lib_hvlite::_jobs::cfg_common::Params {
            local_only: None,
            verbose: ctx.use_parameter(param_verbose.clone()),
            locked: true,
            deny_warnings: true,
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
