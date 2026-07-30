// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publish a standalone OpenVMM source release.
//!
//! OpenVMM releases source first: the published assets are the source archive
//! and its checksums, which a distribution consumes to build and package
//! OpenVMM itself. Prebuilt binaries are a later phase, so this pipeline does
//! not depend on binary signing.

use crate::pipelines_shared::gh_pools;
use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::node::prelude::GhPermission;
use flowey::node::prelude::GhPermissionValue;
use flowey::pipeline::prelude::*;
use flowey_lib_common::git_checkout::RepoSource;

#[derive(clap::Args)]
pub struct ReleaseOpenvmmCli {}

impl IntoPipeline for ReleaseOpenvmmCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        openvmm_release_pipeline(backend_hint)
    }
}

fn openvmm_release_pipeline(backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
    if !matches!(backend_hint, PipelineBackendHint::Github) {
        anyhow::bail!("OpenVMM release pipelines only support the GitHub backend");
    }

    let mut pipeline = Pipeline::new();
    pipeline
        .gh_set_ci_triggers(GhCiTriggers {
            tags: vec!["openvmm-v*".into()],
            ..Default::default()
        })
        .gh_set_name("OpenVMM Release")
        .gh_set_flowey_bootstrap_template(
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

    // Build the archive this pipeline is about to publish, before publishing
    // it. The publishing job assembles from the same commit under the same
    // release identity, and assembly is reproducible, so the archive proved
    // buildable here and the archive uploaded there are the same bytes.
    let distro_build = pipeline
        .new_job(
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
            FlowArch::X86_64,
            "build openvmm [distribution config, x64-linux-gnu]",
        )
        .gh_set_pool(gh_pools::linux_x64_gh())
        .side_effect(
            |done| flowey_lib_hvlite::_jobs::check_distro_build::Request {
                identity:
                    flowey_lib_hvlite::assemble_openvmm_source_release::IdentitySource::ReleaseTag,
                done,
            },
        )
        .finish();

    let publish = pipeline
        .new_job(
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
            FlowArch::X86_64,
            "publish OpenVMM release",
        )
        .gh_set_pool(gh_pools::linux_x64_gh())
        .gh_grant_permissions::<flowey_lib_common::publish_gh_release::Node>([(
            GhPermission::Contents,
            GhPermissionValue::Write,
        )])
        .gh_grant_permissions::<flowey_lib_common::attest_build_provenance::Node>([
            (GhPermission::Contents, GhPermissionValue::Read),
            (GhPermission::IdToken, GhPermissionValue::Write),
            (GhPermission::Attestations, GhPermissionValue::Write),
        ])
        .side_effect(|done| flowey_lib_hvlite::_jobs::publish_openvmm_gh_release::Request { done })
        .finish();
    pipeline.non_artifact_dep(&publish, &distro_build);

    Ok(pipeline)
}
