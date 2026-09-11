// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`BurnInCli`]

use crate::pipelines_shared::gh_pools;
use flowey::node::prelude::GhPermission;
use flowey::node::prelude::GhPermissionValue;
use flowey::pipeline::prelude::*;
use flowey_lib_common::git_checkout::RepoSource;
use flowey_lib_hvlite::_jobs::download_and_run_nextest_vmm_tests::VmmTestsProfile;

/// A pipeline for repeated running VMM tests to determine the frequency of
/// failures
#[derive(clap::Args)]
pub struct BurnInCli {}

impl IntoPipeline for BurnInCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let mut pipeline = Pipeline::new();

        let openvmm_repo_source = match backend_hint {
            PipelineBackendHint::Local => anyhow::bail!("local backend unsupported"),
            PipelineBackendHint::Github => RepoSource::GithubSelf,
            PipelineBackendHint::Ado => anyhow::bail!("ado backend unsupported"),
        };

        if let RepoSource::GithubSelf = &openvmm_repo_source {
            pipeline.gh_set_flowey_bootstrap_template(
                crate::pipelines_shared::gh_flowey_bootstrap_template::get_template(),
            );
        }

        let cfg_common_params = crate::pipelines_shared::cfg_common_params::get_cfg_common_params(
            &mut pipeline,
            backend_hint,
            None,
        )?;

        pipeline
            .gh_set_name("OpenVMM Burn In")
            .gh_add_schedule_trigger(GhScheduleTriggers {
                cron: "0 19 * * 1-5".into(),
                timezone: Some("America/Los_Angeles".into()),
            });

        pipeline.inject_all_jobs_with(move |job| {
            job.dep_on(&cfg_common_params)
                .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
                .dep_on(
                    |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                        hvlite_repo_source: openvmm_repo_source.clone(),
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
                // 15 hour timeout. Should take approximately 8.5 hours.
                .with_timeout_in_minutes(900)
        });

        struct VmmTestJobParams<'a> {
            platform: FlowPlatform,
            arch: FlowArch,
            gh_pool: GhRunner,
            label: &'a str,
            profile: VmmTestsProfile,
        }

        // in the future we will add more burn in tests
        #[expect(clippy::single_element_loop)]
        for VmmTestJobParams {
            platform,
            arch,
            gh_pool,
            label,
            profile,
        } in [VmmTestJobParams {
            platform: FlowPlatform::Windows,
            arch: FlowArch::X86_64,
            gh_pool: gh_pools::windows_snp_self_hosted_baremetal(),
            label: "x64-windows-all-snp",
            profile: VmmTestsProfile::X64WindowsAll,
        }] {
            pipeline
                .new_job(platform, arch, format!("run vmm-tests [{label}]"))
                .gh_set_pool(gh_pool)
                .dep_on(|ctx| {
                    flowey_lib_hvlite::_jobs::download_and_run_nextest_vmm_tests::Params {
                        label: label.into(),
                        profile,
                        repetitions: std::num::NonZeroU64::new(25).unwrap(),
                        done: ctx.new_done_handle(),
                    }
                })
                .finish();
        }

        Ok(pipeline)
    }
}
