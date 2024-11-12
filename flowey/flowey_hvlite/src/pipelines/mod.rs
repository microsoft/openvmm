// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::pipeline::prelude::*;
use restore_packages::RestorePackagesCli;

pub mod build_igvm;
pub mod checkin_gates;
pub mod restore_packages;

#[derive(clap::Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum OpenvmmPipelines {
    /// Alias for root-level `regen` command.
    // DEVNOTE: this enables the useful `cargo xflowey regen` alias
    Regen {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        args: Vec<String>,
    },

    BuildIgvm(build_igvm::BuildIgvmCli),

    /// Flowey pipelines primarily designed to run in CI.
    #[clap(subcommand)]
    Ci(OpenvmmPipelinesCi),

    /// Install tools needed to build OpenVMM
    RestorePackages(RestorePackagesCli),
}

#[derive(clap::Subcommand)]
pub enum OpenvmmPipelinesCi {
    CheckinGates(checkin_gates::CheckinGatesCli),
}

impl IntoPipeline for OpenvmmPipelines {
    fn into_pipeline(self, pipeline_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        match self {
            OpenvmmPipelines::Regen { args } => {
                std::process::Command::new("cargo")
                    .args([
                        "run",
                        "-p",
                        "flowey_hvlite",
                        "--profile",
                        "flowey",
                        "--",
                        "regen",
                    ])
                    .args(args)
                    .spawn()?
                    .wait()?;
                std::process::exit(0)
            }

            OpenvmmPipelines::BuildIgvm(cmd) => cmd.into_pipeline(pipeline_hint),

            OpenvmmPipelines::Ci(cmd) => match cmd {
                OpenvmmPipelinesCi::CheckinGates(cmd) => cmd.into_pipeline(pipeline_hint),
            },
            OpenvmmPipelines::RestorePackages(cmd) => cmd.into_pipeline(pipeline_hint),
        }
    }
}
