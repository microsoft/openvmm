// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::pipelines::vmm_tests_run_target::VmmTestsRunTargetCli;
use cca_tests::CcaTestsCli;
use flowey::pipeline::prelude::*;
use restore_packages::RestorePackagesCli;
use vmm_tests_run::VmmTestsRunCli;

pub mod build_docs;
pub mod build_igvm;
pub mod build_opentmk;
pub mod build_reproducible;
pub mod cca_tests;
pub mod checkin_gates;
pub mod custom_vmfirmwareigvm_dll;
pub mod openvmm_source_release;
pub mod restore_packages;
pub mod vmm_tests_run;
pub mod vmm_tests_run_target;

#[derive(clap::Subcommand)]
#[expect(clippy::large_enum_variant)]
pub enum OpenvmmPipelines {
    /// Alias for root-level `regen` command.
    // DEVNOTE: this enables the useful `cargo xflowey regen` alias
    Regen {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },

    BuildIgvm(build_igvm::BuildIgvmCli),
    /// Build OpenTMK and package it into a bootable VHD
    BuildOpentmk(build_opentmk::BuildOpentmkCli),
    BuildReproducible(build_reproducible::BuildReproducibleCli),
    CustomVmfirmwareigvmDll(custom_vmfirmwareigvm_dll::CustomVmfirmwareigvmDllCli),

    /// Flowey pipelines primarily designed to run in CI.
    #[clap(subcommand)]
    Ci(OpenvmmPipelinesCi),

    /// Install tools needed to build OpenVMM
    RestorePackages(RestorePackagesCli),

    /// Build and run VMM tests with automatic artifact discovery
    VmmTestsRun(VmmTestsRunCli),

    /// Run VMM tests on a target system with artifacts built by `VmmTestsRun`.
    VmmTestsRunTarget(VmmTestsRunTargetCli),

    /// Build and run CCA tests with installation of emulation environment supported
    CcaTests(CcaTestsCli),
}

#[derive(clap::Subcommand)]
pub enum OpenvmmPipelinesCi {
    CheckinGates(checkin_gates::CheckinGatesCli),
    BuildDocs(build_docs::BuildDocsCli),
    /// Assemble, validate, and draft an OpenVMM source release.
    OpenvmmSourceRelease(openvmm_source_release::OpenvmmSourceReleaseCli),
}

impl IntoPipeline for OpenvmmPipelines {
    fn into_pipeline(self, pipeline_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        match self {
            OpenvmmPipelines::Regen { args } => {
                let status = std::process::Command::new("cargo")
                    .args([
                        "run",
                        "-p",
                        "flowey_hvlite",
                        "--profile",
                        "light",
                        "--",
                        "regen",
                    ])
                    .args(args)
                    .spawn()?
                    .wait()?;
                std::process::exit(status.code().unwrap_or(-1));
            }
            OpenvmmPipelines::BuildIgvm(cmd) => cmd.into_pipeline(pipeline_hint),
            OpenvmmPipelines::BuildOpentmk(cmd) => cmd.into_pipeline(pipeline_hint),
            OpenvmmPipelines::BuildReproducible(cmd) => cmd.into_pipeline(pipeline_hint),
            OpenvmmPipelines::CustomVmfirmwareigvmDll(cmd) => cmd.into_pipeline(pipeline_hint),
            OpenvmmPipelines::Ci(cmd) => match cmd {
                OpenvmmPipelinesCi::CheckinGates(cmd) => cmd.into_pipeline(pipeline_hint),
                OpenvmmPipelinesCi::BuildDocs(cmd) => cmd.into_pipeline(pipeline_hint),
                OpenvmmPipelinesCi::OpenvmmSourceRelease(cmd) => cmd.into_pipeline(pipeline_hint),
            },
            OpenvmmPipelines::RestorePackages(cmd) => cmd.into_pipeline(pipeline_hint),
            OpenvmmPipelines::VmmTestsRun(cmd) => cmd.into_pipeline(pipeline_hint),
            OpenvmmPipelines::VmmTestsRunTarget(cmd) => cmd.into_pipeline(pipeline_hint),
            OpenvmmPipelines::CcaTests(cmd) => cmd.into_pipeline(pipeline_hint),
        }
    }
}
