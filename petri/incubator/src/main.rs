// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Standalone CLI for testing the incubator launcher.

use clap::Parser;

/// Standalone CLI for testing the incubator launcher.
#[derive(Parser)]
struct Args {
    /// Path to a TOML profile file.
    #[clap(long)]
    profile: String,
    /// Path to the kernel image (auto-detected if omitted).
    #[clap(long)]
    kernel: Option<std::path::PathBuf>,
    /// Path to the initrd (auto-detected if omitted).
    #[clap(long)]
    initrd: Option<std::path::PathBuf>,
    /// Directory to share with the guest.
    #[clap(long)]
    share: String,
    /// Override the QEMU binary path from the profile.
    #[clap(long)]
    qemu_binary: Option<std::path::PathBuf>,
    /// Timeout in seconds.
    #[clap(long, default_value_t = 1800)]
    timeout: u64,
    /// Command to run in the guest.
    #[clap(last = true, required = true)]
    command: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let profile = incubator::IncubatorProfile::from_file(std::path::Path::new(&args.profile))?;

    let kernel = args
        .kernel
        .unwrap_or_else(|| find_aarch64_kernel().expect("could not find aarch64 kernel"));
    let initrd = args
        .initrd
        .unwrap_or_else(|| find_aarch64_initrd().expect("could not find aarch64 initrd"));

    eprintln!("Profile: {}", args.profile);
    eprintln!("Kernel:  {}", kernel.display());
    eprintln!("Initrd:  {}", initrd.display());
    eprintln!("Share:   {}", args.share);
    eprintln!("Command: {:?}", args.command);
    eprintln!();

    let output = incubator::run_in_incubator(incubator::IncubatorConfig {
        profile,
        kernel,
        initrd,
        share_dir: std::path::PathBuf::from(args.share),
        guest_command: args.command,
        timeout: std::time::Duration::from_secs(args.timeout),
        qemu_binary_override: args.qemu_binary,
    })?;

    eprintln!();
    eprintln!(
        "Completed in {:.1}s, exit code: {:?}",
        output.elapsed.as_secs_f64(),
        output.exit_code
    );

    std::process::exit(output.exit_code.unwrap_or(1));
}

/// Search for an aarch64 kernel in the openvmm deps directory.
fn find_aarch64_kernel() -> Option<std::path::PathBuf> {
    find_in_deps("Image", "aarch64")
}

/// Search for an aarch64 initrd in the openvmm deps directory.
fn find_aarch64_initrd() -> Option<std::path::PathBuf> {
    find_in_deps("initrd", "aarch64")
}

fn find_in_deps(filename: &str, arch_filter: &str) -> Option<std::path::PathBuf> {
    // Walk up to find the repo root (look for Cargo.toml with [workspace])
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists() {
            if let Ok(contents) = std::fs::read_to_string(&cargo_toml) {
                if contents.contains("[workspace]") {
                    break;
                }
            }
        }
        if !dir.pop() {
            return None;
        }
    }

    // Search flowey-persist for the file
    let persist_dir = dir.join("flowey-persist");
    if !persist_dir.exists() {
        return None;
    }

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    collect_files(&persist_dir, filename, arch_filter, &mut candidates);
    candidates.sort();
    candidates.pop() // latest by lexicographic order
}

fn collect_files(
    dir: &std::path::Path,
    filename: &str,
    arch_filter: &str,
    results: &mut Vec<std::path::PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, filename, arch_filter, results);
        } else if path.file_name().is_some_and(|n| n == filename) {
            if path.to_string_lossy().contains(arch_filter) {
                results.push(path);
            }
        }
    }
}
