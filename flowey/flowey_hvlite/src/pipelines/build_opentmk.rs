// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`BuildOpentmkCli`]

use crate::pipelines::build_igvm::bail_if_running_in_ci;
use crate::pipelines_shared::cfg_common_params::CommonArchCli;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::run_cargo_build::common::CommonArch;
use std::path::PathBuf;

/// Build OpenTMK and package it into a bootable VHD.
/// DO NOT USE IN CI.
#[derive(clap::Args)]
pub struct BuildOpentmkCli {
    /// Target architecture for the OpenTMK build.
    ///
    /// Defaults to x86-64.
    #[clap(default_value = "x86-64")]
    pub arch: CommonArchCli,

    /// Custom name for the output binary and VHD.
    ///
    /// The built EFI binary will use this as its output name (via cargo's
    /// `--bin` renaming), and the VHD and PDB files will also use this name.
    /// For example, `--name my_tmk` produces `my_tmk.efi`, `my_tmk.vhd`,
    /// and `my_tmk.pdb`.
    #[clap(long)]
    pub name: Option<String>,

    /// Build using release profile.
    #[clap(long)]
    pub release: bool,

    /// Directory for the output artifacts.
    #[clap(long, default_value = "flowey-out/build-opentmk")]
    pub dir: PathBuf,

    /// pass `--verbose` to cargo
    #[clap(long)]
    pub verbose: bool,

    /// pass `--locked` to cargo
    #[clap(long)]
    pub locked: bool,

    /// Automatically install any missing required dependencies.
    #[clap(long)]
    pub install_missing_deps: bool,
}

impl IntoPipeline for BuildOpentmkCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("build-opentmk is for local use only")
        }

        bail_if_running_in_ci()?;

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );

        let Self {
            arch,
            name,
            release,
            dir,
            verbose,
            locked,
            install_missing_deps,
        } = self;

        let arch: CommonArch = arch.into();

        std::fs::create_dir_all(&dir)?;

        let mut pipeline = Pipeline::new();

        pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "build-opentmk",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
            .dep_on(
                |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                    hvlite_repo_source: openvmm_repo,
                },
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: install_missing_deps,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(verbose),
                locked,
                deny_warnings: false,
                no_incremental: false,
            })
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::local_build_opentmk::Params {
                    artifact_dir: ReadVar::from_static(dir),
                    done: ctx.new_done_handle(),
                    arch,
                    release,
                    name,
                },
            )
            .finish();

        Ok(pipeline)
    }
}
