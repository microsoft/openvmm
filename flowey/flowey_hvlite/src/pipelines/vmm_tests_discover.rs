// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pipeline to discover required artifacts for VMM tests.
//!
//! This builds the vmm_tests binary and queries it for required artifacts,
//! outputting the result as JSON that can be passed to `vmm-tests --artifacts-file`.

use crate::pipelines::vmm_tests::VmmTestTargetCli;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::run_cargo_build::common::CommonTriple;
use std::path::PathBuf;

/// Discover required artifacts for VMM tests
#[derive(clap::Args)]
pub struct VmmTestsDiscoverCli {
    /// Specify what target to build the VMM tests for
    ///
    /// If not specified, defaults to the current host target.
    #[clap(long)]
    target: Option<VmmTestTargetCli>,

    /// Test filter to use when discovering artifacts
    #[clap(long, default_value = "all()")]
    filter: String,

    /// Output file for the discovered artifacts JSON.
    /// If not specified, outputs to stdout.
    #[clap(long)]
    output: Option<PathBuf>,

    /// Release build instead of debug build
    #[clap(long)]
    release: bool,

    /// pass `--verbose` to cargo
    #[clap(long)]
    verbose: bool,
}

impl IntoPipeline for VmmTestsDiscoverCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("vmm-tests-discover is for local use only")
        }

        let Self {
            target,
            filter,
            output,
            release,
            verbose,
        } = self;

        let target = if let Some(t) = target {
            t
        } else {
            match (
                FlowArch::host(backend_hint),
                FlowPlatform::host(backend_hint),
            ) {
                (FlowArch::Aarch64, FlowPlatform::Windows) => VmmTestTargetCli::WindowsAarch64,
                (FlowArch::X86_64, FlowPlatform::Windows) => VmmTestTargetCli::WindowsX64,
                (FlowArch::X86_64, FlowPlatform::Linux(_)) => VmmTestTargetCli::LinuxX64,
                _ => anyhow::bail!("unsupported host"),
            }
        };

        let target_triple = match target {
            VmmTestTargetCli::WindowsAarch64 => CommonTriple::AARCH64_WINDOWS_MSVC,
            VmmTestTargetCli::WindowsX64 => CommonTriple::X86_64_WINDOWS_MSVC,
            VmmTestTargetCli::LinuxX64 => CommonTriple::X86_64_LINUX_GNU,
        };

        // Canonicalize output path to absolute path relative to current working directory
        let output = output.map(|p| {
            if p.is_absolute() {
                p
            } else {
                std::env::current_dir()
                    .expect("failed to get current directory")
                    .join(p)
            }
        });

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );

        let mut pipeline = Pipeline::new();

        pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "discover vmm test artifacts",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
            .dep_on(
                |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                    hvlite_repo_source: openvmm_repo.clone(),
                },
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: false,
                    force_nuget_mono: false,
                    external_nuget_auth: false,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(verbose),
                locked: false,
                deny_warnings: false,
            })
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::local_discover_vmm_tests_artifacts::Params {
                    target: target_triple,
                    filter,
                    output,
                    release,
                    done: ctx.new_done_handle(),
                },
            )
            .finish();

        Ok(pipeline)
    }
}
