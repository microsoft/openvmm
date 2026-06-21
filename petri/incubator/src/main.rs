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
    /// Host directory for logs and captured output.
    #[clap(long)]
    output_dir: Option<std::path::PathBuf>,
    /// Guest path to the pipette binary.
    #[clap(long, default_value = "/share/pipette")]
    guest_pipette: String,
    /// Environment variable to set for the guest command, as KEY=VALUE.
    #[clap(long = "guest-env", value_name = "KEY=VALUE")]
    guest_env: Vec<GuestEnv>,
    /// Working directory for the guest command.
    #[clap(long)]
    guest_current_dir: Option<String>,
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
    let guest_env = args
        .guest_env
        .into_iter()
        .map(|env| (env.key, env.value))
        .collect();

    let output = incubator::run_in_incubator(incubator::IncubatorConfig {
        profile,
        kernel,
        initrd,
        share_dir: args.share.clone().into(),
        output_dir: args
            .output_dir
            .unwrap_or_else(|| std::path::Path::new(&args.share).join("test_results")),
        guest_pipette_path: args.guest_pipette,
        guest_command: args.command,
        guest_env,
        guest_current_dir: args.guest_current_dir,
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

#[derive(Clone)]
struct GuestEnv {
    key: String,
    value: String,
}

impl std::str::FromStr for GuestEnv {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (key, value) = s
            .split_once('=')
            .ok_or_else(|| "expected KEY=VALUE".to_string())?;
        if key.is_empty() {
            return Err("environment variable name must not be empty".to_string());
        }
        Ok(Self {
            key: key.to_string(),
            value: value.to_string(),
        })
    }
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
