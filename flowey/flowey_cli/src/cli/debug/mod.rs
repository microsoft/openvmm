// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod dump_stage0_dag;
pub mod interrogate;
pub mod list_nodes;
pub mod list_patches;

/// Debug commands internal to flowey. Unless you're debugging / developing
/// flowey internals, it's unlikely you'll need to use these.
#[derive(clap::Subcommand)]
pub enum DebugCommands {
    DumpStage0Dag(dump_stage0_dag::DumpStage0Dag),
    Interrogate(interrogate::Interrogate),
    ListNodes(list_nodes::ListNodes),
    ListPatches(list_patches::ListPatches),
}

impl DebugCommands {
    pub fn run(self) -> anyhow::Result<()> {
        match self {
            DebugCommands::Interrogate(cmd) => cmd.run(),
            DebugCommands::ListNodes(cmd) => cmd.run(),
            DebugCommands::ListPatches(cmd) => cmd.run(),
            DebugCommands::DumpStage0Dag(cmd) => cmd.run(),
        }
    }
}
