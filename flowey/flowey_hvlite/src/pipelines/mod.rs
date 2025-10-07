// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::pipeline::prelude::*;
use restore_packages::RestorePackagesCli;
use vmm_tests::VmmTestsCli;

pub mod build_docs;
pub mod build_igvm;
pub mod checkin_gates;
pub mod custom_vmfirmwareigvm_dll;
pub mod restore_packages;
pub mod vmm_tests;

#[derive(clap::Subcommand)]
#[cfg_attr(windows, expect(clippy::large_enum_variant))]
pub enum OpenvmmPipelines {
    /// Alias for root-level `regen` command.
    // DEVNOTE: this enables the useful `cargo xflowey regen` alias
    Regen {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },

    BuildIgvm(build_igvm::BuildIgvmCli),
    CustomVmfirmwareigvmDll(custom_vmfirmwareigvm_dll::CustomVmfirmwareigvmDllCli),

    /// Flowey pipelines primarily designed to run in CI.
    #[clap(subcommand)]
    Ci(OpenvmmPipelinesCi),

    /// Install tools needed to build OpenVMM
    RestorePackages(RestorePackagesCli),

    /// Build and run VMM tests
    VmmTests(VmmTestsCli),
}

#[derive(clap::Subcommand)]
pub enum OpenvmmPipelinesCi {
    CheckinGates(checkin_gates::CheckinGatesCli),
    BuildDocs(build_docs::BuildDocsCli),
}

impl IntoPipeline for OpenvmmPipelines {
    fn into_pipeline(self, pipeline_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        match self {
            OpenvmmPipelines::Regen { args } => {
                let status = std::process::Command::new("cargo")
                    .args(["run", "-p", "flowey_hvlite", "--", "regen"])
                    .args(args)
                    .spawn()?
                    .wait()?;
                std::process::exit(status.code().unwrap_or(-1));
            }
            OpenvmmPipelines::BuildIgvm(cmd) => cmd.into_pipeline(pipeline_hint),
            OpenvmmPipelines::CustomVmfirmwareigvmDll(cmd) => cmd.into_pipeline(pipeline_hint),
            OpenvmmPipelines::Ci(cmd) => match cmd {
                OpenvmmPipelinesCi::CheckinGates(cmd) => cmd.into_pipeline(pipeline_hint),
                OpenvmmPipelinesCi::BuildDocs(cmd) => cmd.into_pipeline(pipeline_hint),
            },
            OpenvmmPipelines::RestorePackages(cmd) => cmd.into_pipeline(pipeline_hint),
            OpenvmmPipelines::VmmTests(cmd) => cmd.into_pipeline(pipeline_hint),
        }
    }
}
