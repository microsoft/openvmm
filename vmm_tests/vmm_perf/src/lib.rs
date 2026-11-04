// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Standalone runner for VMM.Perf profiles.

#![forbid(unsafe_code)]

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
pub fn run() -> anyhow::Result<()> {
    cli::init_tracing();
    runner::VmmPerfRunner::new(cli::Cli::parse())?.run()
}
