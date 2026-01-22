// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`BuildDocsCli`]

use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::node::prelude::GhPermission;
use flowey::node::prelude::GhPermissionValue;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_common::git_checkout::RepoSource;
use flowey_lib_hvlite::run_cargo_build::common::CommonTriple;

#[derive(Copy, Clone, clap::ValueEnum)]
enum PipelineConfig {
    /// Run on all PRs targeting the OpenVMM `main` branch.
    Pr,
    /// Run on all commits that land in OpenVMM's `main` branch.
    ///
    /// The CI pipeline also publishes the guide to openvmm.dev.
    Ci,
}

/// A pipeline defining documentation CI and PR jobs.
#[derive(clap::Args)]
pub struct BuildDocsCli {
    #[clap(long)]
    config: PipelineConfig,

    #[clap(flatten)]
    local_run_args: Option<crate::pipelines_shared::cfg_common_params::LocalRunArgs>,
}

impl IntoPipeline for BuildDocsCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let Self {
            config,
            local_run_args,
        } = self;

        let mut pipeline = Pipeline::new();

        // The docs pipeline should only run on the main branch.
        {
            let branches = vec!["main".into()];
            match config {
                PipelineConfig::Ci => {
                    pipeline
                        .gh_set_ci_triggers(GhCiTriggers {
                            branches,
                            ..Default::default()
                        })
                        .gh_set_name("OpenVMM Docs CI");
                }
                PipelineConfig::Pr => {
                    pipeline
                        .gh_set_pr_triggers(GhPrTriggers {
                            branches,
                            ..GhPrTriggers::new_draftable()
                        })
                        .gh_set_name("OpenVMM Docs PR");
                }
            }
        }

        let openvmm_repo_source = {
            if matches!(backend_hint, PipelineBackendHint::Local) {
                RepoSource::ExistingClone(ReadVar::from_static(crate::repo_root()))
            } else if matches!(backend_hint, PipelineBackendHint::Github) {
                RepoSource::GithubSelf
            } else {
                anyhow::bail!(
                    "Unsupported backend: Docs Pipeline only supports Local and GitHub backends"
                );
            }
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
                .dep_on(|_| {
                    flowey_lib_hvlite::_jobs::cfg_versions::Request::Init(
                        flowey_lib_hvlite::_jobs::cfg_versions::InitParams {
                            should_set_rustup_toolchain: true,
                        },
                    )
                })
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
        });

        // We need to maintain a list of all jobs, so we can hang the "all good"
        // job off of them. This is requires because github status checks only allow
        // specifying jobs, and not workflows.
        // There's more info in the following discussion:
        // <https://github.com/orgs/community/discussions/12395>
        let mut all_jobs = Vec::new();

        // emit mdbook guide build job
        let (pub_guide, use_guide) = pipeline.new_typed_artifact("guide");
        let job = pipeline
            .new_job(
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                FlowArch::X86_64,
                "build mdbook guide",
            )
            .gh_set_pool(crate::pipelines_shared::gh_pools::gh_hosted_x64_linux())
            .dep_on(|ctx| flowey_lib_hvlite::build_guide::Request {
                built_guide: ctx.publish_typed_artifact(pub_guide),
            })
            .finish();

        all_jobs.push(job);

        // emit rustdoc jobs
        let (pub_rustdoc_linux, use_rustdoc_linux) =
            pipeline.new_typed_artifact("x64-linux-rustdoc");
        let (pub_rustdoc_win, use_rustdoc_win) = pipeline.new_typed_artifact("x64-windows-rustdoc");
        for (target, platform, pool, pub_rustdoc) in [
            (
                CommonTriple::X86_64_WINDOWS_MSVC,
                FlowPlatform::Windows,
                crate::pipelines_shared::gh_pools::gh_hosted_x64_windows(),
                pub_rustdoc_win,
            ),
            (
                CommonTriple::X86_64_LINUX_GNU,
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                crate::pipelines_shared::gh_pools::gh_hosted_x64_linux(),
                pub_rustdoc_linux,
            ),
        ] {
            let job = pipeline
                .new_job(
                    platform,
                    FlowArch::X86_64,
                    format!("build and check docs [x64-{platform}]"),
                )
                .gh_set_pool(pool)
                .dep_on(|ctx| flowey_lib_hvlite::build_rustdoc::Request {
                    target_triple: target.as_triple(),
                    docs: ctx.publish_typed_artifact(pub_rustdoc),
                })
                .finish();

            all_jobs.push(job);
        }

        // emit consolidated gh pages publish job
        if matches!(config, PipelineConfig::Ci) {
            let pub_artifact = if matches!(backend_hint, PipelineBackendHint::Local) {
                let (publish, _use) = pipeline.new_typed_artifact("gh-pages");
                Some(publish)
            } else {
                None
            };

            let job = pipeline
                .new_job(FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu), FlowArch::X86_64, "publish openvmm.dev")
                .gh_set_pool(crate::pipelines_shared::gh_pools::gh_hosted_x64_linux())
                .dep_on(
                    |ctx| flowey_lib_hvlite::_jobs::consolidate_and_publish_gh_pages::Params {
                        rustdoc_linux: ctx.use_typed_artifact(&use_rustdoc_linux),
                        rustdoc_windows: ctx.use_typed_artifact(&use_rustdoc_win),
                        guide: ctx.use_typed_artifact(&use_guide),
                        output: if let Some(pub_artifact) = pub_artifact {
                            ctx.publish_typed_artifact(pub_artifact)
                        } else {
                            ctx.new_done_handle().discard_result()
                        }
                    },
                )
                .gh_grant_permissions::<flowey_lib_hvlite::_jobs::consolidate_and_publish_gh_pages::Node>([
                    (GhPermission::IdToken, GhPermissionValue::Write),
                    (GhPermission::Pages, GhPermissionValue::Write),
                ])
                .finish();

            all_jobs.push(job);
        }

        if matches!(config, PipelineConfig::Pr) {
            // Add a job that depends on all others as a workaround for
            // https://github.com/orgs/community/discussions/12395.
            //
            // This workaround then itself requires _another_ workaround, requiring
            // the use of `gh_dangerous_override_if`, and some additional custom job
            // logic, to deal with https://github.com/actions/runner/issues/2566.
            //
            // TODO: Add a way for this job to skip flowey setup and become a true
            // no-op.
            let all_good_job = pipeline
                .new_job(
                    FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                    FlowArch::X86_64,
                    "openvmm build docs gates",
                )
                .gh_set_pool(crate::pipelines_shared::gh_pools::gh_hosted_x64_linux())
                // always run this job, regardless whether or not any previous jobs failed
                .gh_dangerous_override_if("always() && github.event.pull_request.draft == false")
                .gh_dangerous_global_env_var("ANY_JOBS_FAILED", "${{ contains(needs.*.result, 'cancelled') || contains(needs.*.result, 'failure') }}")
                .dep_on(|ctx| flowey_lib_hvlite::_jobs::all_good_job::Params {
                    did_fail_env_var: "ANY_JOBS_FAILED".into(),
                    done: ctx.new_done_handle(),
                })
                .finish();

            for job in all_jobs.iter() {
                pipeline.non_artifact_dep(&all_good_job, job);
            }
        }

        Ok(pipeline)
    }
}
