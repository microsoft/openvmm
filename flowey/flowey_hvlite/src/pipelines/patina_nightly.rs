// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Nightly VMM tests using Patina firmware.

use crate::pipelines_shared::build_and_test::BuildTestConfig;
use crate::pipelines_shared::build_and_test::build_and_test;
use crate::pipelines_shared::gh_pools;
use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::node::prelude::GhPermission;
use flowey::node::prelude::GhPermissionValue;
use flowey::pipeline::prelude::*;
use flowey_lib_common::git_checkout::RepoSource;
use flowey_lib_hvlite::download_uefi_mu_msvm::latest_patina;

/// Run the VMM-test matrix with the latest Patina ClangPDB firmware.
#[derive(clap::Args)]
pub struct PatinaNightlyCli {}

impl IntoPipeline for PatinaNightlyCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Github) {
            anyhow::bail!("Patina nightly requires the GitHub backend");
        }
        let mut pipeline = Pipeline::new();
        pipeline
            .gh_set_name("OpenVMM Patina Nightly")
            .gh_add_schedule_trigger(GhScheduleTriggers {
                cron: "0 19 * * *".into(),
                timezone: Some("America/Los_Angeles".into()),
            });
        pipeline.gh_set_flowey_bootstrap_template(
            crate::pipelines_shared::gh_flowey_bootstrap_template::get_template(),
        );

        let cfg_common_params = crate::pipelines_shared::cfg_common_params::get_cfg_common_params(
            &mut pipeline,
            backend_hint,
            None,
        )?;

        pipeline.inject_all_jobs_with(move |job| {
            job.dep_on(&cfg_common_params)
                .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
                .dep_on(
                    |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                        hvlite_repo_source: RepoSource::GithubSelf,
                    },
                )
                .gh_grant_permissions::<flowey_lib_common::git_checkout::Node>([(
                    GhPermission::Contents,
                    GhPermissionValue::Read,
                )])
                .gh_grant_permissions::<flowey_lib_common::gh_task_azure_login::Node>([(
                    GhPermission::IdToken,
                    GhPermissionValue::Write,
                )])
        });

        let (publish, consume) = pipeline.new_artifact("patina-firmware");
        pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                FlowArch::X86_64,
                "download latest Patina firmware [x64, aarch64]",
            )
            .gh_set_pool(gh_pools::linux_x64_gh())
            .dep_on(|ctx| latest_patina::Request::Download {
                artifact_dir: ctx.publish_artifact(publish),
                done: ctx.new_done_handle(),
            })
            .finish();
        build_and_test(
            pipeline,
            backend_hint,
            BuildTestConfig {
                release: true,
                checkin: None,
                configure_firmware: Some(Box::new(move |job, arch| {
                    job.dep_on(|ctx| latest_patina::Request::UseFirmware {
                        artifact_dir: ctx.use_artifact(&consume),
                        arch,
                    })
                })),
            },
        )
    }
}
