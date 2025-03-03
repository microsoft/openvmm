// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::pipelines_shared::cfg_common_params::CommonArchCli;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::run_cargo_build::common::CommonArch;

/// Download and restore packages needed for building the specified architectures.
#[derive(clap::Args)]
pub struct RestorePackagesCli {
    /// Specify what architectures to restore packages for.
    ///
    /// If none are specified, defaults to just the current host architecture.
    arch: Vec<CommonArchCli>,
}

impl IntoPipeline for RestorePackagesCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );

        let mut pipeline = Pipeline::new();
        let (pub_x64, _use_x64) = pipeline.new_artifact("x64-release-2411-igvm");
        let (pub_aarch64, _use_aarch64) = pipeline.new_artifact("aarch64-release-2411-igvm");
        let mut job = pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "restore packages",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request {})
            .dep_on(
                |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                    hvlite_repo_source: openvmm_repo,
                },
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: true,
                    force_nuget_mono: false,
                    external_nuget_auth: false,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(true),
                locked: false,
                deny_warnings: false,
            });

        let arches: Vec<CommonArchCli> = {
            if self.arch.is_empty() {
                vec![FlowArch::host(backend_hint).try_into()?]
            } else {
                self.arch
            }
        };

        let arches: Vec<CommonArch> = arches.into_iter().map(|arch| arch.into()).collect();

        job = job.dep_on(
            |ctx| flowey_lib_hvlite::_jobs::local_restore_packages::Request {
                arches,
                done: ctx.new_done_handle(),
                aarch64_artifact: ctx.publish_artifact(pub_aarch64),
                x64_artifact: ctx.publish_artifact(pub_x64),
            },
        );

        job.finish();
        Ok(pipeline)
    }
}
