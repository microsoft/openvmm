// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use flowey_core::pipeline::IntoPipeline;
use std::path::Path;

mod cli;
mod flow_resolver;
mod pipeline_resolver;
mod var_db;

/// Entrypoint into generic flowey infrastructure.
pub fn flowey_main<ProjectPipelines: clap::Subcommand + IntoPipeline>(
    flowey_crate: &str,
    repo_root: &Path,
) -> ! {
    if let Err(e) = cli::cli_main::<ProjectPipelines>(flowey_crate, repo_root) {
        log::error!("Error: {:#}", e);
        std::process::exit(-1);
    } else {
        std::process::exit(0)
    }
}

/// Check if we're running inside WSL (Windows Subsystem for Linux).
pub fn running_in_wsl() -> bool {
    let Ok(output) = std::process::Command::new("wslpath")
        .args(["-aw", "/"])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout).starts_with(r"\\wsl.localhost")
}
