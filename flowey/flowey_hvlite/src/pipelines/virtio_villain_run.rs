// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build OpenVMM and run the virtio-villain guest conformance / fault-injection
//! test suite against it.
//!
//! virtio-villain ships as a versioned artifact from the `openvmm-deps` GitHub
//! release. This pipeline downloads that artifact, builds OpenVMM, stages the
//! guest test kernel, and runs the `virtio_villain_tests` nextest suite.
//!
//! Note: this is inert until `openvmm-deps` cuts a release that includes the
//! virtio-villain artifact (and `cfg_versions::OPENVMM_DEPS` is bumped to it).

use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::common::CommonArch;

/// Build and run the virtio-villain test suite against OpenVMM.
#[derive(clap::Args)]
pub struct VirtioVillainRunCli {
    /// Also run known-failing (ignored) villain tests. These are skipped by
    /// default because they exercise OpenVMM bugs (some wedge the VM); enable
    /// this during fix development.
    #[clap(long)]
    pub run_ignored: bool,

    /// Verbose pipeline output.
    #[clap(long)]
    pub verbose: bool,
}

impl IntoPipeline for VirtioVillainRunCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let Self {
            run_ignored,
            verbose,
        } = self;

        // Phase 1 is Linux-only (villain drives OpenVMM under KVM).
        let arch = match (
            FlowArch::host(backend_hint),
            FlowPlatform::host(backend_hint),
        ) {
            (FlowArch::X86_64, FlowPlatform::Linux(_)) => CommonArch::X86_64,
            (FlowArch::Aarch64, FlowPlatform::Linux(_)) => CommonArch::Aarch64,
            _ => anyhow::bail!("virtio-villain tests currently require a Linux host"),
        };

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );

        let mut pipeline = Pipeline::new();

        pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "virtio-villain: run tests",
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
                    auto_install: true,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(verbose),
                locked: false,
                deny_warnings: false,
                no_incremental: false,
            })
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::local_run_virtio_villain::Params {
                    arch,
                    run_ignored,
                    done: ctx.new_done_handle(),
                },
            )
            .finish();

        Ok(pipeline)
    }
}
