// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`OpenvmmSourceReleaseCli`]

use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::node::prelude::GhPermission;
use flowey::node::prelude::GhPermissionValue;
use flowey::pipeline::prelude::*;
use flowey_lib_common::git_checkout::RepoSource;

/// A pipeline that assembles, validates, and drafts an OpenVMM source release.
///
/// This pipeline has no CI or PR triggers. It is dispatched by hand, against a
/// commit whose `[workspace.package] version` a reviewed pull request has
/// already set to the version being released.
///
/// It ends at an `openvmm-v<VERSION>` tag, a vendor archive upload, and a
/// *draft* release. Publishing the reviewed draft stays with a human.
///
/// Only the GitHub backend is supported. The pipeline creates a tag and a
/// release in the upstream repository, so there is nothing meaningful for a
/// local run to do.
#[derive(clap::Args)]
pub struct OpenvmmSourceReleaseCli {}

impl IntoPipeline for OpenvmmSourceReleaseCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let Self {} = self;

        if !matches!(backend_hint, PipelineBackendHint::Github) {
            anyhow::bail!(
                "Unsupported backend: the source release pipeline only supports the GitHub backend"
            );
        }

        let mut pipeline = Pipeline::new();
        pipeline.gh_set_name("OpenVMM Source Release");
        let (publish_release, use_release) = pipeline.new_typed_artifact::<
            flowey_lib_hvlite::assemble_openvmm_vendor_release::VendorReleaseOutput,
        >("openvmm-vendor-release");

        let openvmm_repo_source = RepoSource::GithubSelf;

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
                        hvlite_repo_source: openvmm_repo_source.clone(),
                    },
                )
                .gh_grant_permissions::<flowey_lib_common::git_checkout::Node>([(
                    GhPermission::Contents,
                    GhPermissionValue::Read,
                )])
        });

        let assemble_job = pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                FlowArch::X86_64,
                "assemble openvmm vendor artifact",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::linux_x64_gh())
            .publish(publish_release, |release| {
                flowey_lib_hvlite::assemble_openvmm_vendor_release::Request { release }
            })
            .finish();

        let validate_job = pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                FlowArch::X86_64,
                "validate openvmm distribution build",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::linux_x64_gh())
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::check_distro_build::Request {
                    release: ctx.use_typed_artifact(&use_release),
                    done: ctx.new_done_handle(),
                },
            )
            .finish();

        let publish_job = pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                FlowArch::X86_64,
                "draft openvmm source release",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::linux_x64_gh())
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::publish_openvmm_gh_release::Request {
                    release: ctx.use_typed_artifact(&use_release),
                    done: ctx.new_done_handle(),
                },
            )
            .gh_grant_permissions::<flowey_lib_common::publish_gh_release::Node>([(
                GhPermission::Contents,
                GhPermissionValue::Write,
            )])
            .finish();

        pipeline.non_artifact_dep(&publish_job, &validate_job);
        pipeline.non_artifact_dep(&validate_job, &assemble_job);

        Ok(pipeline)
    }
}
