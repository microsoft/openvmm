// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`OpenvmmReleaseCli`]

use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::node::prelude::GhPermission;
use flowey::node::prelude::GhPermissionValue;
use flowey::pipeline::prelude::*;
use flowey_lib_common::git_checkout::RepoSource;
use flowey_lib_hvlite::common::CommonProfile;
use flowey_lib_hvlite::common::CommonTriple;

/// A pipeline that builds, validates, and drafts an OpenVMM release.
///
/// This pipeline has no CI or PR triggers. It is dispatched by hand, against a
/// commit whose `[workspace.package] version` a reviewed pull request has
/// already set to the version being released.
///
/// It ends at an `openvmm-v<VERSION>` tag and a *draft* release containing the
/// vendor archive and Linux binaries. Unsigned Windows binaries remain
/// workflow artifacts for later signing. Publishing the reviewed draft stays
/// with a human.
///
/// Only the GitHub backend is supported. The pipeline creates a tag and a
/// release in the upstream repository, so there is nothing meaningful for a
/// local run to do.
#[derive(clap::Args)]
pub struct OpenvmmReleaseCli {}

impl IntoPipeline for OpenvmmReleaseCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let Self {} = self;

        if !matches!(backend_hint, PipelineBackendHint::Github) {
            anyhow::bail!(
                "Unsupported backend: the OpenVMM release pipeline only supports the GitHub backend"
            );
        }

        let mut pipeline = Pipeline::new();
        pipeline.gh_set_name("OpenVMM Release");
        let (publish_release, use_release) = pipeline.new_typed_artifact::<
            flowey_lib_hvlite::assemble_openvmm_vendor_release::VendorReleaseOutput,
        >("openvmm-vendor-release");
        let (publish_linux_x64, use_linux_x64) = pipeline
            .new_typed_artifact::<flowey_lib_hvlite::build_openvmm::OpenvmmOutput>(
            "openvmm-x86_64-unknown-linux-musl",
        );
        let (publish_linux_aarch64, use_linux_aarch64) =
            pipeline.new_typed_artifact::<flowey_lib_hvlite::build_openvmm::OpenvmmOutput>(
                "openvmm-aarch64-unknown-linux-musl",
            );
        let (publish_windows_x64, _) = pipeline
            .new_typed_artifact::<flowey_lib_hvlite::build_openvmm::OpenvmmOutput>(
                "openvmm-x86_64-pc-windows-msvc-unsigned",
            );
        let (publish_windows_aarch64, _) = pipeline
            .new_typed_artifact::<flowey_lib_hvlite::build_openvmm::OpenvmmOutput>(
                "openvmm-aarch64-pc-windows-msvc-unsigned",
            );

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

        let linux_x64_job = pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                FlowArch::X86_64,
                "build openvmm [x86_64-unknown-linux-musl]",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::linux_x64_gh())
            .publish(publish_linux_x64, |openvmm| {
                flowey_lib_hvlite::build_openvmm::Request {
                    params: flowey_lib_hvlite::build_openvmm::OpenvmmBuildParams {
                        target: CommonTriple::X86_64_LINUX_MUSL,
                        profile: CommonProfile::Release,
                        features: [flowey_lib_hvlite::build_openvmm::OpenvmmFeature::Tpm].into(),
                    },
                    openvmm,
                }
            })
            .finish();

        let linux_aarch64_job = pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                FlowArch::X86_64,
                "build openvmm [aarch64-unknown-linux-musl]",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::linux_x64_gh())
            .publish(publish_linux_aarch64, |openvmm| {
                flowey_lib_hvlite::build_openvmm::Request {
                    params: flowey_lib_hvlite::build_openvmm::OpenvmmBuildParams {
                        target: CommonTriple::AARCH64_LINUX_MUSL,
                        profile: CommonProfile::Release,
                        features: [flowey_lib_hvlite::build_openvmm::OpenvmmFeature::Tpm].into(),
                    },
                    openvmm,
                }
            })
            .finish();

        let windows_x64_job = pipeline
            .new_job(
                FlowPlatform::Windows,
                FlowArch::X86_64,
                "build unsigned openvmm [x86_64-pc-windows-msvc]",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::windows_x64_gh())
            .publish(publish_windows_x64, |openvmm| {
                flowey_lib_hvlite::build_openvmm::Request {
                    params: flowey_lib_hvlite::build_openvmm::OpenvmmBuildParams {
                        target: CommonTriple::X86_64_WINDOWS_MSVC,
                        profile: CommonProfile::Release,
                        features: Default::default(),
                    },
                    openvmm,
                }
            })
            .finish();

        let windows_aarch64_job = pipeline
            .new_job(
                FlowPlatform::Windows,
                FlowArch::X86_64,
                "build unsigned openvmm [aarch64-pc-windows-msvc]",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::windows_x64_gh())
            .publish(publish_windows_aarch64, |openvmm| {
                flowey_lib_hvlite::build_openvmm::Request {
                    params: flowey_lib_hvlite::build_openvmm::OpenvmmBuildParams {
                        target: CommonTriple::AARCH64_WINDOWS_MSVC,
                        profile: CommonProfile::Release,
                        features: Default::default(),
                    },
                    openvmm,
                }
            })
            .finish();

        let publish_job = pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                FlowArch::X86_64,
                "draft openvmm release",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::linux_x64_gh())
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::publish_openvmm_gh_release::Request {
                    release: ctx.use_typed_artifact(&use_release),
                    linux_x64: ctx.use_typed_artifact(&use_linux_x64),
                    linux_aarch64: ctx.use_typed_artifact(&use_linux_aarch64),
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
        pipeline.non_artifact_dep(&publish_job, &assemble_job);
        pipeline.non_artifact_dep(&publish_job, &linux_x64_job);
        pipeline.non_artifact_dep(&publish_job, &linux_aarch64_job);
        pipeline.non_artifact_dep(&publish_job, &windows_x64_job);
        pipeline.non_artifact_dep(&publish_job, &windows_aarch64_job);

        Ok(pipeline)
    }
}
