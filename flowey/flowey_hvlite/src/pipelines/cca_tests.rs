// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use std::path::PathBuf;

/// CCA test flows, including installing, updating CCA emulation environment and run OpenVMM tests
#[derive(clap::Args)]
pub struct CcaTestsCli {
    /// Root directory for holding all CCA test related stuff
    #[clap(long, default_value = "target/cca-test")]
    pub test_root: PathBuf,

    /// Install CCA emulation environment, including downloading emulator and building all needed firmware
    #[clap(long)]
    pub install_emu: bool,

    /// Update CCA emulation environment by rebuilding firmwares, support a few sub-commands
    #[clap(long)]
    pub update_emu: bool,

    /// Verbose pipeline output
    #[clap(long)]
    pub verbose: bool,

    #[clap(flatten)]
    pub update_emu_subcmds: CcaTestsUpdateEmuSubCmds,
}

#[derive(clap::Args)]
#[clap(next_help_heading = "--update_emu subcommands")]
pub struct CcaTestsUpdateEmuSubCmds {
    /// Rebuild everything. The user can do local modifications, then use this to rebuild the env.
    #[clap(long)]
    pub rebuild: bool,

    /// Update TF-A to specified revision and rebuild.
    #[clap(long)]
    pub tfa_rev: Option<String>,

    /// Update TF-RMM to specified revision and rebuild.
    #[clap(long)]
    pub tfrmm_rev: Option<String>,

    /// Update plane0 Linux to specified revision and rebuild.
    #[clap(long)]
    pub plane0_linux_rev: Option<String>,
}

impl IntoPipeline for CcaTestsCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        let Self {
            test_root,
            install_emu,
            update_emu,
            verbose,
            update_emu_subcmds:
                CcaTestsUpdateEmuSubCmds {
                    rebuild,
                    tfa_rev,
                    tfrmm_rev,
                    plane0_linux_rev,
                },
        } = self;

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );

        // Absolute path is expected across cca_tests infrastructure. Relative
        // paths are resolved from repo root.
        let test_root = if test_root.is_absolute() {
            test_root
        } else {
            crate::repo_root().join(test_root)
        };

        let mut pipeline = Pipeline::new();

        if install_emu {
            let check_job = pipeline
                .new_job(
                    FlowPlatform::host(backend_hint),
                    FlowArch::host(backend_hint),
                    "cca-tests: check existence of emulation envionrment needed tools",
                )
                .dep_on(
                    |ctx| flowey_lib_hvlite::_jobs::local_check_cca_emu_prereq::Params {
                        done: ctx.new_done_handle(),
                    },
                )
                .finish();

            let install_job = pipeline
                .new_job(
                    FlowPlatform::host(backend_hint),
                    FlowArch::host(backend_hint),
                    "cca-tests: install emulation environment",
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
                    |ctx| flowey_lib_hvlite::_jobs::local_install_cca_emu::Params {
                        test_root: test_root.clone(),
                        done: ctx.new_done_handle(),
                    },
                )
                .finish();

            pipeline.non_artifact_dep(&install_job, &check_job);
            return Ok(pipeline);
        }

        let update_job = if update_emu {
            Some(
                pipeline
                    .new_job(
                        FlowPlatform::host(backend_hint),
                        FlowArch::host(backend_hint),
                        "cca-tests: update emulation environment",
                    )
                    .dep_on(
                        |ctx| flowey_lib_hvlite::_jobs::local_update_cca_emu::Params {
                            test_root: test_root.clone(),
                            sub_cmds: flowey_lib_hvlite::_jobs::local_update_cca_emu::SubCmds {
                                rebuild,
                                tfa_rev,
                                tfrmm_rev,
                                plane0_linux_rev,
                            },
                            done: ctx.new_done_handle(),
                        },
                    )
                    .finish(),
            )
        } else {
            None
        };

        let test_job = pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "cca-tests: run cca tests",
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
            .dep_on(|ctx| flowey_lib_hvlite::_jobs::local_run_cca_test::Params {
                test_root: test_root.clone(),
                done: ctx.new_done_handle(),
            })
            .finish();

        // Only add dependency if update_job exists
        if let Some(update_job) = &update_job {
            pipeline.non_artifact_dep(&test_job, update_job);
        }

        Ok(pipeline)
    }
}
