// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`CheckinGatesCli`]

use crate::pipelines_shared::ado_pools;
use crate::pipelines_shared::gh_pools;
use flowey::node::prelude::AdoResourcesRepositoryId;
use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::node::prelude::GhPermission;
use flowey::node::prelude::GhPermissionValue;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_common::git_checkout::RepoSource;
use flowey_lib_hvlite::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use flowey_lib_hvlite::build_vmgstool::VmgstoolOutput;
use flowey_lib_hvlite::common::CommonArch;
use flowey_lib_hvlite::common::CommonPlatform;
use flowey_lib_hvlite::common::CommonProfile;
use flowey_lib_hvlite::common::CommonTriple;
use std::collections::BTreeMap;
use target_lexicon::Triple;

#[derive(Copy, Clone, clap::ValueEnum)]
pub(crate) enum PipelineConfig {
    /// Run on all PRs targeting the OpenVMM GitHub repo.
    Pr,
    /// Run on all commits that land in a branch.
    ///
    /// The key difference between the CI and PR pipelines is whether things are
    /// being built in `release` mode.
    Ci,
    /// Release variant of the `Pr` pipeline.
    PrRelease,
}

/// A unified pipeline defining all checkin gates required to land a commit in
/// the OpenVMM repo.
#[derive(clap::Args)]
pub struct CheckinGatesCli {
    /// Which pipeline configuration to use.
    #[clap(long)]
    config: PipelineConfig,

    #[clap(flatten)]
    local_run_args: Option<crate::pipelines_shared::cfg_common_params::LocalRunArgs>,
}

impl IntoPipeline for CheckinGatesCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let Self {
            config,
            local_run_args,
        } = self;

        let mut pipeline = Pipeline::new();

        // configure pr/ci branch triggers and add gh pipeline name
        {
            let branches = vec!["main".into(), "release/*".into()];

            // Paths that don't affect the Rust build or tests. Changes
            // to only these paths will not trigger the CI pipeline on push.
            //
            // NOTE: The PR pipeline intentionally does NOT use paths-ignore,
            // because the "openvmm checkin gates" job is a required status
            // check. If the workflow is skipped due to path filters, the
            // gate is never reported and the PR is blocked. The CI pipeline
            // can still use paths-ignore since it has no required checks.
            let ci_paths_ignore = vec!["Guide/**".into(), "petri/logview/**".into()];

            match config {
                PipelineConfig::Ci => {
                    pipeline
                        .gh_set_ci_triggers(GhCiTriggers {
                            branches,
                            paths_ignore: ci_paths_ignore.clone(),
                            ..Default::default()
                        })
                        .gh_set_name("OpenVMM CI");
                }
                PipelineConfig::Pr => {
                    pipeline
                        .gh_set_pr_triggers(GhPrTriggers {
                            branches,
                            ..GhPrTriggers::new_draftable()
                        })
                        .gh_set_name("OpenVMM PR")
                        .ado_set_pr_triggers(AdoPrTriggers {
                            branches: vec!["main".into(), "release/*".into(), "embargo/*".into()],
                            exclude_paths: ci_paths_ignore.clone(),
                            ..Default::default()
                        });
                }
                PipelineConfig::PrRelease => {
                    // This workflow is triggered when a specific label is present on a PR.
                    let mut triggers = GhPrTriggers::new_draftable();
                    triggers.branches = branches;
                    triggers.types.push("labeled".into());
                    pipeline
                        .gh_set_pr_triggers(triggers)
                        .gh_set_name("[Optional] OpenVMM Release PR");
                }
            }
        }

        let openvmm_repo_source = match backend_hint {
            PipelineBackendHint::Local => {
                RepoSource::ExistingClone(ReadVar::from_static(crate::repo_root()))
            }
            PipelineBackendHint::Github => RepoSource::GithubSelf,
            PipelineBackendHint::Ado => {
                RepoSource::AdoResource(AdoResourcesRepositoryId::new_self())
            }
        };

        if let RepoSource::GithubSelf = &openvmm_repo_source {
            pipeline.gh_set_flowey_bootstrap_template(
                crate::pipelines_shared::gh_flowey_bootstrap_template::get_template(),
            );
        }

        if let RepoSource::AdoResource(source) = &openvmm_repo_source {
            pipeline.ado_set_flowey_bootstrap_template(
                crate::pipelines_shared::ado_flowey_bootstrap_template::get_template_ado(source),
            );
        }

        let cfg_common_params = crate::pipelines_shared::cfg_common_params::get_cfg_common_params(
            &mut pipeline,
            backend_hint,
            local_run_args,
        )?;

        pipeline.inject_all_jobs_with(move |job| {
            let mut job = job
                .dep_on(&cfg_common_params)
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
                )]);

            // For the release pipeline, only run if the "release-ci-required" label is present and PR is not draft
            if matches!(config, PipelineConfig::PrRelease) {
                job = job.gh_dangerous_override_if(
                    "contains(github.event.pull_request.labels.*.name, 'release-ci-required') && github.event.pull_request.draft == false",
                );
            }

            job
        });

        crate::pipelines_shared::build_and_test::build_and_test(
            pipeline,
            backend_hint,
            crate::pipelines_shared::build_and_test::BuildTestConfig {
                release: !matches!(config, PipelineConfig::Pr),
                checkin: Some(config),
                configure_firmware: None,
            },
        )
    }
}

impl PipelineConfig {
    pub(crate) fn initial_jobs(
        self,
        pipeline: &mut Pipeline,
        release: bool,
        all_jobs: &mut Vec<PipelineJobHandle>,
    ) -> Option<PipelineJobHandle> {
        // Quick check gate
        //
        // Combined fmt + clippy on one self-hosted linux machine.
        // Catches the most common failures quickly before fanning out expensive jobs.
        let quick_check_job = if matches!(self, Self::Pr | Self::PrRelease) {
            let job = pipeline
                .new_job(
                    FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                    FlowArch::X86_64,
                    "quick check [fmt, clippy x64-linux]",
                )
                .gh_set_pool(gh_pools::default_linux())
                .ado_set_pool(ado_pools::default_linux())
                // 1. xtask fmt (linux)
                .side_effect(|done| flowey_lib_hvlite::_jobs::check_xtask_fmt::Request {
                    target: CommonTriple::X86_64_LINUX_GNU,
                    done,
                })
                // 2. clippy for x64-linux-gnu
                .side_effect(|done| flowey_lib_hvlite::_jobs::check_clippy::Request {
                    target: target_lexicon::triple!("x86_64-unknown-linux-gnu"),
                    profile: CommonProfile::from_release(release),
                    done,
                    also_check_misc_nostd_crates: false,
                })
                .finish();

            Some(job)
        } else {
            // skip in CI
            None
        };

        // emit xtask fmt job
        {
            let windows_fmt_job = pipeline
                .new_job(
                    FlowPlatform::Windows,
                    FlowArch::X86_64,
                    "xtask fmt (windows)",
                )
                .gh_set_pool(gh_pools::windows_x64_gh())
                .ado_set_pool(ado_pools::default_windows())
                .side_effect(|done| flowey_lib_hvlite::_jobs::check_xtask_fmt::Request {
                    target: CommonTriple::X86_64_WINDOWS_MSVC,
                    done,
                })
                .finish();

            let linux_fmt_job = if let Some(ref qc) = quick_check_job {
                // PR/PrRelease: linux fmt is handled by the quick-check job
                qc.clone()
            } else {
                // CI mode: keep standalone linux fmt job
                let job = pipeline
                    .new_job(
                        FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                        FlowArch::X86_64,
                        "xtask fmt (linux)",
                    )
                    .gh_set_pool(gh_pools::linux_x64_gh())
                    .ado_set_pool(ado_pools::default_linux())
                    .side_effect(|done| flowey_lib_hvlite::_jobs::check_xtask_fmt::Request {
                        target: CommonTriple::X86_64_LINUX_GNU,
                        done,
                    })
                    .finish();
                all_jobs.push(job.clone());
                job
            };

            // cut down on extra noise by having the linux check run first, and
            // then if it passes, run the windows checks just in case there is a
            // difference between the two.
            pipeline.non_artifact_dep(&windows_fmt_job, &linux_fmt_job);

            all_jobs.push(windows_fmt_job);
        }

        quick_check_job
    }

    pub(crate) fn clippy_unit_tests(
        self,
        pipeline: &mut Pipeline,
        backend_hint: PipelineBackendHint,
        release: bool,
        quick_check_job: &Option<PipelineJobHandle>,
        all_jobs: &mut Vec<PipelineJobHandle>,
    ) {
        let openhcl_musl_target = |arch| {
            CommonTriple::Common {
                arch,
                platform: CommonPlatform::LinuxMusl,
            }
            .as_triple()
        };
        // Emit clippy + unit-test jobs
        //
        // The only reason we bundle clippy and unit-tests together is to avoid
        // requiring another build agent.
        struct ClippyUnitTestJobParams<'a> {
            platform: FlowPlatform,
            arch: FlowArch,
            gh_pool: GhRunner,
            ado_pool: Option<AdoPool>,
            clippy_targets: Option<(&'a str, &'a [(Triple, bool)])>,
            unit_test_target: Option<(&'a str, Triple)>,
        }

        let macos_clippy_targets = [(target_lexicon::triple!("aarch64-apple-darwin"), false)];
        let x64_linux_macos_clippy_targets = [
            (target_lexicon::triple!("x86_64-unknown-linux-gnu"), false),
            (target_lexicon::triple!("aarch64-apple-darwin"), false),
        ];

        for ClippyUnitTestJobParams {
            platform,
            arch,
            gh_pool,
            ado_pool,
            clippy_targets,
            unit_test_target,
        } in [
            ClippyUnitTestJobParams {
                platform: FlowPlatform::Windows,
                arch: FlowArch::X86_64,
                gh_pool: gh_pools::windows_intel_v6_1es(),
                ado_pool: Some(ado_pools::windows_amd_v6_1es()),
                clippy_targets: Some((
                    "x64-windows",
                    &[(target_lexicon::triple!("x86_64-pc-windows-msvc"), false)],
                )),
                unit_test_target: Some((
                    "x64-windows",
                    target_lexicon::triple!("x86_64-pc-windows-msvc"),
                )),
            },
            ClippyUnitTestJobParams {
                platform: FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                arch: FlowArch::X86_64,
                gh_pool: gh_pools::linux_intel_v6_1es(),
                ado_pool: Some(ado_pools::linux_amd_v6_1es()),
                clippy_targets: if quick_check_job.is_some() {
                    // quick check already ran clippy for x64-linux;
                    // still need macos cross-clippy here.
                    Some(("macos", macos_clippy_targets.as_slice()))
                } else {
                    Some((
                        "x64-linux, macos",
                        x64_linux_macos_clippy_targets.as_slice(),
                    ))
                },
                unit_test_target: Some((
                    "x64-linux",
                    target_lexicon::triple!("x86_64-unknown-linux-gnu"),
                )),
            },
            ClippyUnitTestJobParams {
                platform: FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                arch: FlowArch::X86_64,
                gh_pool: gh_pools::linux_intel_v6_1es(),
                ado_pool: Some(ado_pools::linux_amd_v6_1es()),
                clippy_targets: Some((
                    "x64-linux-musl, misc nostd",
                    &[(openhcl_musl_target(CommonArch::X86_64), true)],
                )),
                unit_test_target: Some(("x64-linux-musl", openhcl_musl_target(CommonArch::X86_64))),
            },
            ClippyUnitTestJobParams {
                platform: FlowPlatform::Windows,
                arch: FlowArch::Aarch64,
                gh_pool: gh_pools::windows_arm_v6_1es(),
                ado_pool: None,
                clippy_targets: Some((
                    "aarch64-windows",
                    &[(target_lexicon::triple!("aarch64-pc-windows-msvc"), false)],
                )),
                unit_test_target: Some((
                    "aarch64-windows",
                    target_lexicon::triple!("aarch64-pc-windows-msvc"),
                )),
            },
            ClippyUnitTestJobParams {
                platform: FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                arch: FlowArch::Aarch64,
                gh_pool: gh_pools::linux_arm_v5_1es(),
                ado_pool: None,
                clippy_targets: Some((
                    "aarch64-linux",
                    &[(target_lexicon::triple!("aarch64-unknown-linux-gnu"), false)],
                )),
                unit_test_target: Some((
                    "aarch64-linux",
                    target_lexicon::triple!("aarch64-unknown-linux-gnu"),
                )),
            },
            ClippyUnitTestJobParams {
                platform: FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                arch: FlowArch::Aarch64,
                gh_pool: gh_pools::linux_arm_v5_1es(),
                ado_pool: None,
                clippy_targets: Some((
                    "aarch64-linux-musl, misc nostd",
                    &[(openhcl_musl_target(CommonArch::Aarch64), true)],
                )),
                unit_test_target: Some((
                    "aarch64-linux-musl",
                    openhcl_musl_target(CommonArch::Aarch64),
                )),
            },
        ] {
            // Skip unsupported jobs on ADO backend
            if matches!(backend_hint, PipelineBackendHint::Ado) && ado_pool.is_none() {
                continue;
            }

            let mut job_name = Vec::new();
            if let Some((label, _)) = &clippy_targets {
                job_name.push(format!("clippy [{label}]"));
            }
            if let Some((label, _)) = &unit_test_target {
                job_name.push(format!("unit tests [{label}]"));
            }
            let job_name = job_name.join(", ");

            let unit_test_target = unit_test_target.map(|(label, target)| {
                let test_label = format!("{label}-unit-tests");
                let pub_unit_test_junit_xml = if matches!(backend_hint, PipelineBackendHint::Local)
                {
                    Some(pipeline.new_artifact(&test_label).0)
                } else {
                    None
                };
                (test_label, target, pub_unit_test_junit_xml)
            });

            let mut clippy_unit_test_job = pipeline
                .new_job(platform, arch, job_name)
                .gh_set_pool(gh_pool);

            if let Some(pool) = ado_pool {
                clippy_unit_test_job = clippy_unit_test_job.ado_set_pool(pool);
            }

            if let Some((_, targets)) = clippy_targets {
                for (target, also_check_misc_nostd_crates) in targets {
                    clippy_unit_test_job = clippy_unit_test_job.side_effect(|done| {
                        flowey_lib_hvlite::_jobs::check_clippy::Request {
                            target: target.clone(),
                            profile: CommonProfile::from_release(release),
                            done,
                            also_check_misc_nostd_crates: *also_check_misc_nostd_crates,
                        }
                    });
                }
            }

            if let Some((test_label, target, pub_unit_test_junit_xml)) = unit_test_target {
                clippy_unit_test_job = clippy_unit_test_job
                    .dep_on(|ctx| {
                        flowey_lib_hvlite::_jobs::build_and_run_nextest_unit_tests::Params {
                            junit_test_label: test_label,
                            nextest_profile:
                                flowey_lib_hvlite::run_cargo_nextest_run::NextestProfile::Ci,
                            fail_job_on_test_fail: true,
                            target: target.clone(),
                            profile: CommonProfile::from_release(release),
                            artifact_dir: pub_unit_test_junit_xml.map(|x| ctx.publish_artifact(x)),
                            done: ctx.new_done_handle(),
                        }
                    })
                    .side_effect(|done| {
                        flowey_lib_hvlite::_jobs::build_and_run_doc_tests::Params {
                            target,
                            profile: CommonProfile::from_release(release),
                            done,
                        }
                    });
            }

            all_jobs.push(clippy_unit_test_job.finish());
        }
    }

    pub(crate) fn check_openhcl_size(
        self,
        pipeline: &mut Pipeline,
        backend_hint: PipelineBackendHint,
        arch: CommonArch,
        job_name: String,
        all_jobs: &mut Vec<PipelineJobHandle>,
    ) {
        let arch_tag = match arch {
            CommonArch::X86_64 => "x64",
            CommonArch::Aarch64 => "aarch64",
        };
        // TODO: Once we have a few runs of the openvmm-mirror PR pipeline, this job can be re-worked to use ADO artifacts instead of GH artifacts.
        if matches!(self, Self::Pr) && !matches!(backend_hint, PipelineBackendHint::Ado) {
            let job = pipeline
                .new_job(
                    FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                    FlowArch::X86_64,
                    format!("verify openhcl binary size [{}]", arch_tag),
                )
                .gh_set_pool(gh_pools::linux_x64_gh())
                .side_effect(
                    |done| flowey_lib_hvlite::_jobs::check_openvmm_hcl_size::Request {
                        target: CommonTriple::Common {
                            arch,
                            platform: CommonPlatform::LinuxMusl,
                        },
                        done,
                        pipeline_name: "openvmm-ci.yaml".into(),
                        job_name,
                    },
                );
            all_jobs.push(job.finish());
        }
    }

    pub(crate) fn finish_jobs(
        self,
        pipeline: &mut Pipeline,
        backend_hint: PipelineBackendHint,
        mut all_jobs: Vec<PipelineJobHandle>,
        quick_check_job: Option<PipelineJobHandle>,
        vmgstools: BTreeMap<String, UseTypedArtifact<VmgstoolOutput>>,
    ) {
        // test the flowey local backend by running cargo xflowey build-igvm on x64
        {
            if matches!(backend_hint, PipelineBackendHint::Github) {
                let job = pipeline
                    .new_job(
                        FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                        FlowArch::X86_64,
                        "test flowey local backend",
                    )
                    .gh_set_pool(gh_pools::linux_x64_gh())
                    .side_effect(|done| {
                        flowey_lib_hvlite::_jobs::test_local_flowey_build_igvm::Request {
                            base_recipe: OpenhclIgvmRecipe::X64,
                            done,
                        }
                    });
                all_jobs.push(job.finish());
            }
        }

        // Build the vendored source tree without the repository's
        // `.packages/` provisioning, as a Linux distribution would.
        {
            let distro_build_job = pipeline
                .new_job(
                    FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                    FlowArch::X86_64,
                    "build openvmm [distribution config, x64-linux-gnu]",
                )
                .gh_set_pool(gh_pools::linux_x64_gh())
                .ado_set_pool(ado_pools::default_linux())
                .side_effect(|done| {
                    flowey_lib_hvlite::_jobs::check_distro_build_from_checkout::Request { done }
                })
                .finish();

            all_jobs.push(distro_build_job);
        }

        // all jobs depend on the quick-check gate
        if let Some(ref quick_check) = quick_check_job {
            for job in all_jobs.iter() {
                pipeline.non_artifact_dep(job, quick_check);
            }
            all_jobs.push(quick_check.clone());
        }

        if matches!(self, Self::Pr) && matches!(backend_hint, PipelineBackendHint::Github) {
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
                    "openvmm checkin gates",
                )
                .gh_set_pool(gh_pools::linux_x64_gh())
                // always run this job, regardless whether or not any previous jobs failed
                .gh_dangerous_override_if("always() && github.event.pull_request.draft == false")
                .gh_dangerous_global_env_var("ANY_JOBS_FAILED", "${{ contains(needs.*.result, 'cancelled') || contains(needs.*.result, 'failure') }}")
                .side_effect(|done| flowey_lib_hvlite::_jobs::all_good_job::Params {
                    did_fail_env_var: "ANY_JOBS_FAILED".into(),
                    done,
                })
                .finish();

            for job in all_jobs.iter() {
                pipeline.non_artifact_dep(&all_good_job, job);
            }
        }

        if matches!(self, Self::Ci) && matches!(backend_hint, PipelineBackendHint::Github) {
            let publish_vmgstool_job = pipeline
                .new_job(
                    FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
                    FlowArch::X86_64,
                    "publish vmgstool",
                )
                .gh_grant_permissions::<flowey_lib_common::publish_gh_release::Node>([(
                    GhPermission::Contents,
                    GhPermissionValue::Write,
                )])
                .gh_set_pool(gh_pools::linux_x64_gh())
                .dep_on(
                    |ctx| flowey_lib_hvlite::_jobs::publish_vmgstool_gh_release::Request {
                        vmgstools: vmgstools
                            .into_iter()
                            .map(|(t, v)| (t, ctx.use_typed_artifact(&v)))
                            .collect(),
                        done: ctx.new_done_handle(),
                    },
                )
                .finish();

            // All other jobs must succeed in order to publish
            for job in all_jobs.iter() {
                pipeline.non_artifact_dep(&publish_vmgstool_job, job);
            }
        }
    }
}
