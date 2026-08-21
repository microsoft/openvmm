// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Standalone runner for VMM.Perf profiles.

#![forbid(unsafe_code)]

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
compile_error!("vmm_perf currently supports Linux and Windows hosts only");

mod cli;
mod command;
mod config;
mod diagnostics;
mod host;
mod runner;
mod runtime;
#[cfg(test)]
mod test_support;
mod virtual_client;

use clap::Parser as _;

/// Parses CLI arguments and runs all requested VMM.Perf profiles/configurations.
pub fn main() -> anyhow::Result<()> {
    cli::init_tracing();
    runner::VmmPerfRunner::new(cli::Cli::parse())?.run()
}
