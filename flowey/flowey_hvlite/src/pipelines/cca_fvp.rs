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

        // Store the original dir value for validation before canonicalization
        let original_dir = dir.clone();

        // Convert dir to absolute path to ensure consistency across jobs
        // Relative paths are resolved from the repository root
        let dir = std::fs::canonicalize(&dir)
            .or_else(|_| {
                // If dir doesn't exist yet, make it absolute relative to repo root
                let abs = if dir.is_absolute() {
                    dir.clone()
                } else {
                    crate::repo_root().join(&dir)
                };
                Ok::<_, anyhow::Error>(abs)
            })?;

        // Put Shrinkwrap repo under the pipeline working dir, so it's self-contained.
        let shrinkwrap_dir = dir.join("shrinkwrap");
        let shrinkwrap_config_dir = shrinkwrap_dir.join("config");

        // Helper to resolve platform/overlay paths:
        // - Absolute paths: use as-is
        // - Simple filenames (no '/'): resolve to <dir>/shrinkwrap/config/
        // - Relative paths with '/': must start with --dir prefix
        let resolve_config_path = |p: PathBuf, arg_name: &str| -> anyhow::Result<PathBuf> {
            if p.is_absolute() {
                Ok(p)
            } else {
                let p_str = p.to_string_lossy();

                // Check if it's a simple filename (no directory separators)
                if !p_str.contains('/') {
                    // Simple filename: resolve to shrinkwrap/config/
                    return Ok(shrinkwrap_config_dir.join(p));
                }

                // It's a relative path with directories - validate it starts with --dir
                let original_dir_str = original_dir.to_string_lossy();
                let dir_prefix = original_dir_str.trim_start_matches("./");
                let alt_dir_prefix = format!("./{}", dir_prefix);

                if p_str.starts_with(dir_prefix) || p_str.starts_with(&alt_dir_prefix) {
                    // Valid: path starts with --dir prefix
                    // Strip the prefix and reconstruct using the canonical dir
                    let stripped = p_str.strip_prefix(dir_prefix)
                        .or_else(|| p_str.strip_prefix(alt_dir_prefix.as_str()))
                        .unwrap()
                        .trim_start_matches('/');

                    Ok(dir.join(stripped))
                } else {
                    // Invalid: relative path doesn't start with --dir
                    anyhow::bail!(
                        "Relative path for {} must start with the --dir value ({}). Got: {}. \
                         Either use an absolute path, a simple filename, or a relative path starting with '{}/'.",
                        arg_name, original_dir.display(), p.display(), original_dir_str
                    )
                }
            }
        };

        // Resolve platform YAML path
        let platform = resolve_config_path(platform, "--platform")?;

        // Resolve overlay YAML paths
        let overlay: Vec<PathBuf> = overlay.into_iter()
            .map(|p| resolve_config_path(p, "--overlay"))
            .collect::<anyhow::Result<Vec<_>>>()?;

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
