// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Standalone CLI for testing the incubator launcher.

#![forbid(unsafe_code)]

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

    let arch = profile.incubator.arch();
    let kernel = match args.kernel {
        Some(kernel) => kernel,
        None => kernel_or_initrd_from_env(arch, "OPENVMM_LINUX_DIRECT_KERNEL")?,
    };
    let initrd = match args.initrd {
        Some(initrd) => initrd,
        None => kernel_or_initrd_from_env(arch, "OPENVMM_LINUX_DIRECT_INITRD")?,
    };

    tracing::info!(profile = %args.profile, "profile");
    tracing::info!(kernel = %kernel.display(), "kernel");
    tracing::info!(initrd = %initrd.display(), "initrd");
    tracing::info!(share = %args.share, "share");
    tracing::info!(command = ?args.command, "command");

    let output = incubator::run_in_incubator(incubator::IncubatorConfig {
        profile,
        kernel,
        initrd,
        share_dir: std::path::PathBuf::from(args.share),
        guest_command: args.command,
        timeout: std::time::Duration::from_secs(args.timeout),
        qemu_binary_override: args.qemu_binary,
    })?;

    tracing::info!(
        elapsed_secs = output.elapsed.as_secs_f64(),
        exit_code = ?output.exit_code,
        "completed"
    );

    std::process::exit(output.exit_code.unwrap_or(1));
}

/// Resolve a kernel or initrd path from an environment variable.
///
/// Mimics openvmm's lookup: given a base name like `OPENVMM_LINUX_DIRECT_KERNEL`,
/// it checks the unprefixed variable first, then the arch-specific variant
/// (e.g. `AARCH64_OPENVMM_LINUX_DIRECT_KERNEL`). The arch comes from the
/// profile. These variables are set by the repo's `.cargo/config.toml` so that
/// `cargo run` picks up the sample kernel/initrd packaged alongside
/// openvmm-deps. If neither is set, fail with a hint to pass the path
/// explicitly.
fn kernel_or_initrd_from_env(
    arch: incubator::Arch,
    base_name: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let prefixed = format!("{}_{base_name}", arch.env_prefix());
    let value = non_empty_env(base_name).or_else(|| non_empty_env(&prefixed));
    match value {
        Some(value) => Ok(std::path::PathBuf::from(value)),
        None => anyhow::bail!(
            "neither {base_name} nor {prefixed} is set (normally provided by \
             .cargo/config.toml); pass --kernel/--initrd explicitly or run via \
             cargo from the repo"
        ),
    }
}

/// Read an environment variable, treating an empty value as unset.
fn non_empty_env(var: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(var).filter(|value| !value.is_empty())
}
