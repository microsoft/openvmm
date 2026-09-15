// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run VMM tests on a target system with artifacts built by `VmmTestsRun`.

use crate::pipelines::vmm_tests_run::VmmTestTargetCli;
use crate::pipelines::vmm_tests_run::resolve_incubator;
use crate::pipelines::vmm_tests_run::resolve_target;
use anyhow::Context;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::init_vmm_tests_env::PetriParams;
use flowey_lib_hvlite::install_vmm_tests_external_deps::VmmTestsExternalDeps;
use flowey_lib_hvlite::install_vmm_tests_external_deps::VmmTestsExternalDepsLinux;
use flowey_lib_hvlite::install_vmm_tests_external_deps::VmmTestsExternalDepsWindows;
use std::num::NonZeroU64;
use std::path::PathBuf;
use vmm_test_images::KnownTestArtifacts;

/// Run VMM tests on a target system with artifacts built by `VmmTestsRun`.
#[derive(clap::Args)]
pub struct VmmTestsRunTargetCli {
    /// Specify what target to build the VMM tests for
    ///
    /// If not specified, defaults to the current host target.
    #[clap(long)]
    target: Option<VmmTestTargetCli>,

    /// Directory for the output artifacts.
    #[clap(long)]
    dir: PathBuf,

    /// Test filter (nextest filter expression)
    #[clap(long, default_value = "all()")]
    filter: String,

    /// The test artifacts to download.
    #[clap(long, value_delimiter = ',')]
    artifacts: Vec<KnownTestArtifacts>,

    /// Prep steps variants to run
    #[clap(long, value_delimiter = ',')]
    prep_steps: Vec<String>,

    /// pass `--verbose` to cargo
    #[clap(long)]
    verbose: bool,

    /// Automatically install any missing required dependencies.
    #[clap(long)]
    install_missing_deps: bool,

    /// Skip the interactive VHD download prompt
    #[clap(long)]
    skip_vhd_prompt: bool,

    /// use the nextest CI profile rather than the default one
    #[clap(long)]
    ci_profile: bool,

    /// Don't reuse prepped vhds, even if they already exist.
    /// Use when making changes to prep_steps
    #[clap(long)]
    no_reuse_prepped_vhds: bool,

    /// Whether the tests selected require hardware isolation
    #[clap(long)]
    needs_hardware_isolation: bool,

    /// Whether the tests selected require the test igvm agent
    #[clap(long)]
    needs_igvm_agent: bool,

    /// How many times to run the tests
    #[clap(long)]
    repetitions: Option<u64>,

    /// Run tests inside an emulated incubator.
    #[clap(long, num_args = 0..=1)]
    #[expect(clippy::option_option)]
    incubator: Option<Option<PathBuf>>,
}

impl IntoPipeline for VmmTestsRunTargetCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("vmm-tests-run-target is for local use only")
        }

        let Self {
            target,
            dir,
            filter,
            artifacts,
            prep_steps,
            verbose,
            install_missing_deps,
            skip_vhd_prompt,
            ci_profile,
            no_reuse_prepped_vhds,
            needs_hardware_isolation,
            needs_igvm_agent,
            repetitions,
            incubator,
        } = self;

        // When --incubator is set, --target must also be specified
        // to indicate the cross-compilation target for the incubator.
        if incubator.is_some() && target.is_none() {
            anyhow::bail!("--incubator requires --target (e.g., --target linux-aarch64-musl)");
        }

        let repetitions =
            NonZeroU64::new(repetitions.unwrap_or(1)).context("repetitions must not be zero")?;

        let target = resolve_target(target, backend_hint)?;

        let incubator_profile = incubator
            .map(|i| resolve_incubator(i, &target))
            .transpose()?;

        let external_deps = match target.as_triple().operating_system {
            target_lexicon::OperatingSystem::Windows => {
                VmmTestsExternalDeps::Windows(VmmTestsExternalDepsWindows {
                    hyperv: true, // TODO
                    whp: true,    // TODO
                    hardware_isolation: needs_hardware_isolation,
                })
            }
            target_lexicon::OperatingSystem::Linux => {
                VmmTestsExternalDeps::Linux(VmmTestsExternalDepsLinux {
                    hugetlb_2mb_overcommit_pages: None, // TODO
                    prepare_vhost_vsock: false,         // TODO
                })
            }
            _ => unreachable!(),
        };

        let mut pipeline = Pipeline::new();

        let mut job = pipeline.new_job(
            FlowPlatform::host(backend_hint),
            FlowArch::host(backend_hint),
            "run vmm tests on target system",
        );

        job = job.dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init);

        job = job
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: install_missing_deps,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(verbose),
                locked: false,
                deny_warnings: false,
                no_incremental: false,
            })
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::local_run_nextest_vmm_tests::Params {
                    target,
                    test_content_dir: dir,
                    filter,
                    downloaded_artifacts: artifacts,
                    external_deps,
                    prep_steps_variants: prep_steps,
                    needs_test_igvm_agent_rpc_server: needs_igvm_agent,
                    skip_vhd_prompt,
                    nextest_profile: if ci_profile {
                        flowey_lib_hvlite::run_cargo_nextest_run::NextestProfile::Ci
                    } else {
                        flowey_lib_hvlite::run_cargo_nextest_run::NextestProfile::Default
                    },
                    petri_params: PetriParams {
                        disable_remote_artifacts: false,
                        reuse_prepped_vhds: !no_reuse_prepped_vhds,
                        require_2mb_hugetlb: false, // TODO
                    },
                    repetitions,
                    incubator_profile,
                    done: ctx.new_done_handle(),
                },
            );

        job.finish();

        Ok(pipeline)
    }
}
