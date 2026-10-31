// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generate a candidate sandbox profile from worker trace JSONL files.

#![forbid(unsafe_code)]

use clap::Parser;
use sandbox_profiler::TraceOptions;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "sandbox_profiler")]
#[command(about = "Generate a candidate OpenVMM sandbox profile from worker traces")]
struct Options {
    /// Directory containing worker JSONL trace files.
    trace_dir: PathBuf,

    /// Worker type/name, such as vm or dpm.
    worker: String,

    /// Override the output directory. Defaults to TRACE_DIR.
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let options = Options::parse();
    let output_dir = options
        .output_dir
        .unwrap_or_else(|| options.trace_dir.clone());
    let trace_report = sandbox_profiler::build_profile(&TraceOptions {
        trace_dir: options.trace_dir,
        worker: options.worker,
        output_dir,
    })?;

    println!("generated profile: {}", trace_report.profile_path.display());
    println!("trace files: {}", trace_report.trace_files.len());

    if trace_report.observed_filesystem.is_empty() {
        println!("filesystem accesses: none observed");
    } else {
        println!("filesystem accesses observed:");
        for observation in &trace_report.observed_filesystem {
            println!("  - {observation}");
        }
    }

    if trace_report.observed_network.is_empty() {
        println!("network accesses: none observed");
    } else {
        println!("network accesses observed:");
        for observation in &trace_report.observed_network {
            println!("  - {observation}");
        }
    }

    println!(
        "observed syscalls: {}",
        trace_report.observed_syscalls.len()
    );

    println!("read-only grants: {}", trace_report.policy_read_paths.len());
    println!(
        "read-write grants: {}",
        trace_report.policy_read_write_paths.len()
    );
    if !trace_report.policy_filesystem_access_omissions.is_empty() {
        println!("filesystem grant omissions:");
        for omission in &trace_report.policy_filesystem_access_omissions {
            println!("  - {omission}");
        }
    }

    println!(
        "generated network policy: {:?}",
        trace_report.policy_network
    );
    println!(
        "generated syscall denials: {}",
        trace_report.policy_syscalls.join(", ")
    );
    Ok(())
}
