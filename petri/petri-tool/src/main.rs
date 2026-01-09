// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tool for using petri functionality from the command line.

#![forbid(unsafe_code)]

use anyhow::Context as _;
use clap::Parser;
use petri::ArtifactResolver;
use petri::TestArtifactRequirements;

/// Command line interface for petri-related tasks.
#[derive(Parser)]
struct CliArgs {
    #[clap(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Builds a cloud-init disk image for a Linux VM image.
    ///
    /// This image creates a petri user with the password "petri".
    CloudInit {
        /// The architecture of the VM image.
        #[clap(long)]
        arch: MachineArch,

        /// Path to the output disk image.
        output: std::path::PathBuf,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum MachineArch {
    #[clap(name = "x86_64")]
    X86_64,
    Aarch64,
}

fn main() -> anyhow::Result<()> {
    let args = CliArgs::parse();
    match args.command {
        Command::CloudInit { output, arch } => {
            let image = build_with_artifacts(|resolver: ArtifactResolver<'_>| {
                petri::disk_image::AgentImage::new(petri_artifacts_common::tags::OsFlavor::Linux)
                    .with_pipette(
                        &resolver,
                        match arch {
                            MachineArch::X86_64 => {
                                petri_artifacts_common::tags::MachineArch::X86_64
                            }
                            MachineArch::Aarch64 => {
                                petri_artifacts_common::tags::MachineArch::Aarch64
                            }
                        },
                    )
            })?;

            let disk = image
                .build(petri::disk_image::ImageType::Raw)
                .context("failed to build disk image")?
                .context("no files for the this platform")?;
            disk.persist(output)
                .context("failed to persist disk image")?;

            Ok(())
        }
    }
}

fn build_with_artifacts<R>(mut f: impl FnMut(ArtifactResolver<'_>) -> R) -> anyhow::Result<R> {
    let resolver =
        petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new("");
    let mut requirements = TestArtifactRequirements::new();
    f(ArtifactResolver::collector(&mut requirements));
    let artifacts = requirements.resolve(&resolver)?;
    Ok(f(ArtifactResolver::resolver(&artifacts)))
}
