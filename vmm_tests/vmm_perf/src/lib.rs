// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Standalone runner for VMM.Perf profiles.

#![forbid(unsafe_code)]

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod cli;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod command;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod config;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod diagnostics;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod host;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod runner;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod runtime;
#[cfg(all(test, any(target_os = "linux", target_os = "windows")))]
mod test_support;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod virtual_client;

#[cfg(any(target_os = "linux", target_os = "windows"))]
use clap::Parser as _;

/// Parses CLI arguments and runs all requested VMM.Perf profiles/configurations.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn main() -> anyhow::Result<()> {
    cli::init_tracing();
    runner::VmmPerfRunner::new(cli::Cli::parse())?.run()
}

/// Reports that VMM.Perf cannot run on this host platform.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn main() -> anyhow::Result<()> {
    anyhow::bail!("VMM.Perf supports Linux and Windows hosts only")
}
