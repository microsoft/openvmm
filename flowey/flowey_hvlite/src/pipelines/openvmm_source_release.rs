// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`OpenvmmSourceReleaseCli`]

use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::node::prelude::GhPermission;
use flowey::node::prelude::GhPermissionValue;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_common::git_checkout::RepoSource;

/// A pipeline that assembles, validates, and drafts an OpenVMM source release.
///
/// This pipeline has no CI or PR triggers. It is dispatched by hand, against a
/// commit whose `[workspace.package] version` a reviewed pull request has
/// already set to the version being released.
///
/// It ends at a *draft* release. Publishing that draft is what creates the
/// `openvmm-v<VERSION>` tag, and that step stays with a human.
#[derive(clap::Args)]
pub struct OpenvmmSourceReleaseCli {
    #[clap(flatten)]
    local_run_args: Option<crate::pipelines_shared::cfg_common_params::LocalRunArgs>,
}

impl IntoPipeline for OpenvmmSourceReleaseCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let Self { local_run_args } = self;

        let mut pipeline = Pipeline::new();
        pipeline.gh_set_name("OpenVMM Source Release");

        let openvmm_repo_source = if matches!(backend_hint, PipelineBackendHint::Local) {
            RepoSource::ExistingClone(ReadVar::from_static(crate::repo_root()))
        } else if matches!(backend_hint, PipelineBackendHint::Github) {
            RepoSource::GithubSelf
        } else {
            anyhow::bail!(
                "Unsupported backend: the source release pipeline only supports Local and GitHub backends"
            );
        };

        if let RepoSource::GithubSelf = &openvmm_repo_source {
            pipeline.gh_set_flowey_bootstrap_template(
                crate::pipelines_shared::gh_flowey_bootstrap_template::get_template(),
            );
        }

        let cfg_common_params = crate::pipelines_shared::cfg_common_params::get_cfg_common_params(
            &mut pipeline,
            backend_hint,
            local_run_args,
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

        // Assemble once. The archive travels to the remaining jobs as an
        // artifact so that what gets validated is what gets published.
        let (pub_source_release, use_source_release) =
            pipeline.new_typed_artifact("openvmm-source-release");

        let assemble_job = pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                FlowArch::X86_64,
                "assemble openvmm source release",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::linux_x64_gh())
            .publish(pub_source_release, |release| {
                flowey_lib_hvlite::assemble_openvmm_source_release::Request { release }
            })
            .finish();

        // Build the archive exactly as a distribution would, before anyone is
        // offered a draft to publish.
        let validate_job = pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                FlowArch::X86_64,
                "build openvmm [distribution config, x64-linux-gnu]",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::linux_x64_gh())
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::check_distro_build::Request {
                    release: ctx.use_typed_artifact(&use_source_release),
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
                    release: ctx.use_typed_artifact(&use_source_release),
                    done: ctx.new_done_handle(),
                },
            )
            .gh_grant_permissions::<flowey_lib_common::publish_gh_release::Node>([(
                GhPermission::Contents,
                GhPermissionValue::Write,
            )])
            .gh_grant_permissions::<flowey_lib_common::attest_build_provenance::Node>([
                (GhPermission::Contents, GhPermissionValue::Read),
                (GhPermission::IdToken, GhPermissionValue::Write),
                (GhPermission::Attestations, GhPermissionValue::Write),
                (GhPermission::ArtifactMetadata, GhPermissionValue::Write),
            ])
            .finish();

        // The draft job consumes the same artifact as the gate, so nothing
        // orders them by data alone. Publication must wait for validation.
        pipeline.non_artifact_dep(&publish_job, &validate_job);
        pipeline.non_artifact_dep(&validate_job, &assemble_job);

        Ok(pipeline)
    }
}
