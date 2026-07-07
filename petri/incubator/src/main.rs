// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Standalone CLI for testing the incubator launcher.

#![forbid(unsafe_code)]

use anyhow::Context;
use clap::Parser;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::PathBuf;

/// Standalone CLI for launching the incubator.
///
/// Every option can also be supplied via its `INCUBATOR_*` environment
/// variable, which is how flowey drives this binary as a cargo-nextest target
/// runner: cargo invokes `incubator <test-binary> <args>` with no flags, and
/// the configuration is read from the environment.
#[derive(Parser)]
struct Args {
    /// Path to a TOML profile file.
    #[clap(long, env = "INCUBATOR_PROFILE")]
    profile: String,
    /// Path to the kernel image (auto-detected if omitted).
    #[clap(long, env = "INCUBATOR_KERNEL")]
    kernel: Option<PathBuf>,
    /// Path to the initrd (auto-detected if omitted).
    #[clap(long, env = "INCUBATOR_INITRD")]
    initrd: Option<PathBuf>,
    /// Directory to share with the guest.
    #[clap(long, env = "INCUBATOR_SHARE")]
    share: String,
    /// Host directory for logs and captured output.
    #[clap(long, env = "INCUBATOR_OUTPUT_DIR")]
    output_dir: Option<PathBuf>,
    /// Guest path to the pipette binary.
    #[clap(long, env = "INCUBATOR_GUEST_PIPETTE")]
    guest_pipette: Option<String>,
    /// Environment variable to set for the guest command, as KEY=VALUE.
    #[clap(long = "guest-env", value_name = "KEY=VALUE")]
    guest_env: Vec<GuestEnv>,
    /// Map the command path from the host share to the guest share.
    #[clap(long, env = "INCUBATOR_MAP_COMMAND_PATH")]
    map_command_path: bool,
    /// Working directory for the guest command.
    #[clap(long, env = "INCUBATOR_GUEST_CURRENT_DIR")]
    guest_current_dir: Option<String>,
    /// Override the QEMU binary path from the profile.
    #[clap(long, env = "INCUBATOR_QEMU_BINARY")]
    qemu_binary: Option<PathBuf>,
    /// Do not allocate a PTY for the guest command or put the host terminal
    /// into raw mode. Set automatically when running as a cargo-nextest target
    /// runner, where raw mode would interfere with nextest's Ctrl-C handling.
    #[clap(long, env = "INCUBATOR_NO_PTY")]
    no_pty: bool,
    /// Timeout in seconds.
    #[clap(long, env = "INCUBATOR_TIMEOUT", default_value_t = 1800)]
    timeout: u64,
    /// Command to run in the guest: the program followed by its arguments.
    ///
    /// May be preceded by `--`, but it is not required, so that cargo-nextest
    /// can invoke us as `incubator <test-binary> <args>`.
    #[clap(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    command: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let share_dir = PathBuf::from(&args.share);
    let path_mapper = incubator::HostPathMapper::new(&share_dir, incubator::GUEST_SHARE_ROOT)?;
    let guest_pipette = args
        .guest_pipette
        .unwrap_or_else(|| format!("{}/pipette", incubator::GUEST_SHARE_ROOT));

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
    let mut command = args.command;
    if args.map_command_path {
        let host_command = command.first().context("empty guest command")?.clone();
        command[0] = path_mapper.map_path(&host_command).with_context(|| {
            format!("failed to map command path '{host_command}' into the guest share")
        })?;
    }

    tracing::info!(share = %share_dir.display(), "share");
    tracing::info!(command = ?command, "command");

    let mut guest_env = guest_env_from_incubator_env(&path_mapper)?;
    for env in args.guest_env {
        guest_env.insert(env.key, env.value);
    }

    let output = incubator::run_in_incubator(incubator::IncubatorConfig {
        profile,
        kernel,
        initrd,
        share_dir: share_dir.clone(),
        output_dir: args
            .output_dir
            .unwrap_or_else(|| share_dir.join("test_results")),
        guest_pipette_path: guest_pipette,
        guest_command: command,
        guest_env,
        guest_current_dir: args.guest_current_dir,
        timeout: std::time::Duration::from_secs(args.timeout),
        qemu_binary_override: args.qemu_binary,
        // Only drive an interactive PTY when stdin is a real terminal and the
        // caller hasn't opted out (cargo-nextest sets --no-pty).
        allocate_pty: !args.no_pty && std::io::stdin().is_terminal(),
    })?;

    tracing::info!(
        elapsed_secs = output.elapsed.as_secs_f64(),
        exit_code = ?output.exit_code,
        "completed"
    );

    std::process::exit(output.exit_code.unwrap_or(1));
}

fn guest_env_from_incubator_env(
    path_mapper: &incubator::HostPathMapper,
) -> anyhow::Result<BTreeMap<String, String>> {
    let policy = match std::env::var("INCUBATOR_ENV") {
        Ok(policy) => policy,
        Err(std::env::VarError::NotPresent) => return Ok(BTreeMap::new()),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("INCUBATOR_ENV is not valid UTF-8")
        }
    };

    let host_env = std::env::vars().collect();
    incubator::guest_env_from_incubator_env(&policy, &host_env, path_mapper)
        .context("failed to apply INCUBATOR_ENV")
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
fn kernel_or_initrd_from_env(arch: incubator::Arch, base_name: &str) -> anyhow::Result<PathBuf> {
    let prefixed = format!("{}_{base_name}", arch.env_prefix());
    let value = non_empty_env(base_name).or_else(|| non_empty_env(&prefixed));
    match value {
        Some(value) => Ok(PathBuf::from(value)),
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
