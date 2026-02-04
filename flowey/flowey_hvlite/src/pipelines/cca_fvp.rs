// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use std::path::PathBuf;

/// Install Shrinkwrap, Build + run CCA FVP via Shrinkwrap (local)
#[derive(clap::Args)]
pub struct CcaFvpCli {
    /// Directory for output artifacts/logs (pipeline working dir)
    #[clap(long)]
    pub dir: PathBuf,

    /// Platform YAML (e.g. cca-3world.yaml)
    #[clap(long)]
    pub platform: PathBuf,

    /// Overlay YAMLs (repeatable), e.g. --overlay buildroot.yaml --overlay planes.yaml
    #[clap(long)]
    pub overlay: Vec<PathBuf>,

    /// Build-time variables (repeatable), e.g. --btvar 'GUEST_ROOTFS=${artifact:BUILDROOT}'
    #[clap(long)]
    pub btvar: Vec<String>,

    /// Rootfs path to pass at runtime, e.g.
    /// --rootfs /abs/path/.shrinkwrap/package/cca-3world/rootfs.ext2
    #[clap(long)]
    pub rootfs: PathBuf,

    /// Additional runtime variables (repeatable), besides ROOTFS, e.g. --rtvar FOO=bar
    #[clap(long)]
    pub rtvar: Vec<String>,

    /// Extra args appended to `shrinkwrap build` (escape hatch)
    #[clap(long)]
    pub build_arg: Vec<String>,

    /// Extra args appended to `shrinkwrap run` (escape hatch)
    #[clap(long)]
    pub run_arg: Vec<String>,

    /// Timeout in seconds for `shrinkwrap run`
    #[clap(long, default_value_t = 600)]
    pub timeout_sec: u64,

    /// Automatically install missing deps (requires sudo on Ubuntu)
    #[clap(long)]
    pub install_missing_deps: bool,

    /// If repo already exists, attempt `git pull --ff-only`
    #[clap(long, default_value_t = true)]
    pub update_shrinkwrap_repo: bool,

    /// Verbose pipeline output
    #[clap(long)]
    pub verbose: bool,
}

impl IntoPipeline for CcaFvpCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let Self {
            dir,
            platform,
            overlay,
            btvar,
            rootfs,
            rtvar,
            build_arg,
            run_arg: _,
            timeout_sec: _,
            install_missing_deps,
            update_shrinkwrap_repo,
            verbose,
        } = self;

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );

        let mut pipeline = Pipeline::new();

        // Convert dir to absolute path to ensure consistency across jobs
        let dir = std::fs::canonicalize(&dir)
            .or_else(|_| {
                // If dir doesn't exist yet, make it absolute relative to current dir
                let abs = if dir.is_absolute() {
                    dir.clone()
                } else {
                    std::env::current_dir()?.join(&dir)
                };
                Ok::<_, anyhow::Error>(abs)
            })?;

        // Put Shrinkwrap repo under the pipeline working dir, so it's self-contained.
        let shrinkwrap_dir = dir.join("shrinkwrap");

        // Convert platform and overlay paths that reference the shrinkwrap directory
        // to absolute paths, since shrinkwrap will change directory during execution
        let platform = if platform.starts_with("target/cca-fvp/shrinkwrap/") ||
                          platform.starts_with("./target/cca-fvp/shrinkwrap/") {
            // This is a shrinkwrap config file, make it absolute
            let rel_path = platform.strip_prefix("target/cca-fvp/shrinkwrap/")
                .or_else(|_| platform.strip_prefix("./target/cca-fvp/shrinkwrap/"))
                .unwrap();
            shrinkwrap_dir.join(rel_path)
        } else if platform.is_absolute() {
            platform
        } else {
            // Try to canonicalize if it exists, otherwise make it absolute
            std::fs::canonicalize(&platform).unwrap_or_else(|_| {
                std::env::current_dir().unwrap().join(&platform)
            })
        };

        let overlay: Vec<PathBuf> = overlay.into_iter().map(|p| {
            if p.starts_with("target/cca-fvp/shrinkwrap/") ||
               p.starts_with("./target/cca-fvp/shrinkwrap/") {
                // This is a shrinkwrap config file, make it absolute
                let rel_path = p.strip_prefix("target/cca-fvp/shrinkwrap/")
                    .or_else(|_| p.strip_prefix("./target/cca-fvp/shrinkwrap/"))
                    .unwrap();
                shrinkwrap_dir.join(rel_path)
            } else if p.is_absolute() {
                p
            } else {
                // Try to canonicalize if it exists, otherwise make it absolute
                std::fs::canonicalize(&p).unwrap_or_else(|_| {
                    std::env::current_dir().unwrap().join(&p)
                })
            }
        }).collect();

        // Create separate jobs to ensure proper ordering
        let install_job = pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "cca-fvp: install shrinkwrap",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                hvlite_repo_source: openvmm_repo.clone(),
            })
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: install_missing_deps,
                    force_nuget_mono: false,
                    external_nuget_auth: false,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(verbose),
                locked: false,
                deny_warnings: false,
            })
            .dep_on(|ctx| flowey_lib_hvlite::_jobs::local_install_shrinkwrap::Params {
                shrinkwrap_dir: shrinkwrap_dir.clone(),
                do_installs: install_missing_deps,
                update_repo: update_shrinkwrap_repo,
                done: ctx.new_done_handle(),
            })
            .finish();

        let build_job = pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "cca-fvp: shrinkwrap build",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                hvlite_repo_source: openvmm_repo.clone(),
            })
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: install_missing_deps,
                    force_nuget_mono: false,
                    external_nuget_auth: false,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(verbose),
                locked: false,
                deny_warnings: false,
            })
            .dep_on(|ctx| flowey_lib_hvlite::_jobs::local_shrinkwrap_build::Params {
                out_dir: dir.clone(),
                shrinkwrap_dir: shrinkwrap_dir.clone(),
                platform_yaml: platform.clone(),
                overlays: overlay.clone(),
                btvars: btvar.clone(),
                extra_args: build_arg.clone(),
                done: ctx.new_done_handle(),
            })
            .finish();

        // Shrinkwrap run job
        let run_job = pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "cca-fvp: shrinkwrap run",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                hvlite_repo_source: openvmm_repo.clone(),
            })
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: install_missing_deps,
                    force_nuget_mono: false,
                    external_nuget_auth: false,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(verbose),
                locked: false,
                deny_warnings: false,
            })
            .dep_on(|ctx| flowey_lib_hvlite::_jobs::local_shrinkwrap_run::Params {
                out_dir: dir.clone(),
                shrinkwrap_dir: shrinkwrap_dir.clone(),
                platform_yaml: platform.clone(),
                rootfs_path: rootfs.clone(),
                rtvars: rtvar.clone(),
                done: ctx.new_done_handle(),
            })
            .finish();

        // Explicitly declare job dependencies
        pipeline.non_artifact_dep(&build_job, &install_job);
        pipeline.non_artifact_dep(&run_job, &build_job);
        Ok(pipeline)
    }
}
