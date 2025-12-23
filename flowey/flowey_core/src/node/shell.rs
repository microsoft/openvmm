// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! FloweyShell: A wrapper around xshell::Shell that automatically wraps commands
//! in nix-shell when running on the Nix platform.

use crate::node::{FlowPlatform, FlowPlatformLinuxDistro};
use std::collections::BTreeMap;
use std::ops::Deref;

/// A wrapper around `xshell::Shell` that automatically wraps commands in nix-shell
/// when the platform is configured to use Nix.
///
/// This allows pipeline-level configuration of nix usage without requiring individual
/// command sites to handle nix-shell wrapping manually.
pub struct FloweyShell {
    inner: xshell::Shell,
    platform: FlowPlatform,
}

impl FloweyShell {
    /// Create a new FloweyShell for the given platform.
    ///
    /// The shell will automatically wrap commands in nix-shell if the platform
    /// is `FlowPlatform::Linux(FlowPlatformLinuxDistro::Nix)`.
    pub fn new(platform: FlowPlatform) -> Result<Self, xshell::Error> {
        Ok(Self {
            inner: xshell::Shell::new()?,
            platform,
        })
    }

    /// Get a reference to the inner xshell::Shell.
    ///
    /// This can be used for accessing the shell's methods directly,
    /// such as `change_dir`, `current_dir`, etc.
    pub fn inner(&self) -> &xshell::Shell {
        &self.inner
    }

    /// Check if commands should be wrapped in nix-shell.
    fn needs_nix_wrapper(&self) -> bool {
        // Only wrap in nix-shell if the platform is Nix AND we're not already in a nix-shell
        // IN_NIX_SHELL is set when we're already inside a nix-shell (scenario 2)
        // USING_NIX=1 is set when we want to use nix-shell but aren't in one yet (scenario 3)
        matches!(
            self.platform,
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Nix)
        ) && std::env::var("IN_NIX_SHELL").is_err()
    }

    /// Run a command with the given arguments and environment variables.
    ///
    /// If the platform is Nix, the command will be automatically wrapped in
    /// `nix-shell --pure --run`.
    ///
    /// # Arguments
    /// * `program` - The program to run (e.g., "cargo")
    /// * `args` - Arguments to pass to the program
    /// * `env_vars` - Environment variables to set for the command
    pub fn run_cmd(
        &self,
        program: &str,
        args: &[String],
        env_vars: &BTreeMap<String, String>,
    ) -> Result<(), xshell::Error> {
        if self.needs_nix_wrapper() {
            self.run_in_nix_shell(program, args, env_vars)
        } else {
            let mut cmd = xshell::cmd!(self.inner, "{program} {args...}");
            cmd = cmd.envs(env_vars);
            cmd.run()
        }
    }

    /// Run a command and capture its stdout as a String.
    ///
    /// If the platform is Nix, the command will be automatically wrapped in
    /// `nix-shell --pure --run`.
    ///
    /// # Arguments
    /// * `program` - The program to run (e.g., "cargo")
    /// * `args` - Arguments to pass to the program
    /// * `env_vars` - Environment variables to set for the command
    pub fn read_cmd(
        &self,
        program: &str,
        args: &[String],
        env_vars: &BTreeMap<String, String>,
    ) -> Result<String, xshell::Error> {
        if self.needs_nix_wrapper() {
            self.read_in_nix_shell(program, args, env_vars)
        } else {
            let mut cmd = xshell::cmd!(self.inner, "{program} {args...}");
            cmd = cmd.envs(env_vars);
            cmd.read()
        }
    }

    /// Run a command and capture its output (stdout, stderr, exit code).
    ///
    /// If the platform is Nix, the command will be automatically wrapped in
    /// `nix-shell --pure --run`.
    ///
    /// # Arguments
    /// * `program` - The program to run (e.g., "cargo")
    /// * `args` - Arguments to pass to the program
    /// * `env_vars` - Environment variables to set for the command
    pub fn output_cmd(
        &self,
        program: &str,
        args: &[String],
        env_vars: &BTreeMap<String, String>,
    ) -> Result<std::process::Output, xshell::Error> {
        if self.needs_nix_wrapper() {
            self.output_in_nix_shell(program, args, env_vars)
        } else {
            let mut cmd = xshell::cmd!(self.inner, "{program} {args...}");
            cmd = cmd.envs(env_vars);
            cmd.output()
        }
    }

    /// Execute a command wrapped in nix-shell.
    fn run_in_nix_shell(
        &self,
        program: &str,
        args: &[String],
        env_vars: &BTreeMap<String, String>,
    ) -> Result<(), xshell::Error> {
        let full_cmd = build_shell_command(program, args, env_vars);
        xshell::cmd!(self.inner, "nix-shell --pure --run {full_cmd}").run()
    }

    /// Execute a command wrapped in nix-shell and capture stdout.
    fn read_in_nix_shell(
        &self,
        program: &str,
        args: &[String],
        env_vars: &BTreeMap<String, String>,
    ) -> Result<String, xshell::Error> {
        let full_cmd = build_shell_command(program, args, env_vars);
        xshell::cmd!(self.inner, "nix-shell --pure --run {full_cmd}").read()
    }

    /// Execute a command wrapped in nix-shell and capture output.
    fn output_in_nix_shell(
        &self,
        program: &str,
        args: &[String],
        env_vars: &BTreeMap<String, String>,
    ) -> Result<std::process::Output, xshell::Error> {
        let full_cmd = build_shell_command(program, args, env_vars);
        xshell::cmd!(self.inner, "nix-shell --pure --run {full_cmd}").output()
    }
}

impl Deref for FloweyShell {
    type Target = xshell::Shell;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Build a shell command string from program, args, and environment variables.
///
/// The resulting string is safe to pass to xshell::cmd! with {variable} interpolation,
/// which will handle proper shell escaping.
fn build_shell_command(
    program: &str,
    args: &[String],
    env_vars: &BTreeMap<String, String>,
) -> String {
    // Build environment prefix (KEY=value KEY2=value2 ...)
    let env_prefix = env_vars
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");

    // Build command line (program arg1 arg2 ...)
    let cmd_line = if args.is_empty() {
        program.to_string()
    } else {
        format!("{} {}", program, args.join(" "))
    };

    // Combine environment and command
    if env_prefix.is_empty() {
        cmd_line
    } else {
        format!("{env_prefix} {cmd_line}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_shell_command_simple() {
        let cmd = build_shell_command("cargo", &["build".into()], &BTreeMap::new());
        assert_eq!(cmd, "cargo build");
    }

    #[test]
    fn test_build_shell_command_no_args() {
        let cmd = build_shell_command("cargo", &[], &BTreeMap::new());
        assert_eq!(cmd, "cargo");
    }

    #[test]
    fn test_build_shell_command_multiple_args() {
        let args = vec!["build".into(), "--release".into(), "--verbose".into()];
        let cmd = build_shell_command("cargo", &args, &BTreeMap::new());
        assert_eq!(cmd, "cargo build --release --verbose");
    }

    #[test]
    fn test_build_shell_command_with_env() {
        let mut env = BTreeMap::new();
        env.insert("RUST_LOG".into(), "debug".into());
        let cmd = build_shell_command("cargo", &["build".into()], &env);
        assert_eq!(cmd, "RUST_LOG=debug cargo build");
    }

    #[test]
    fn test_build_shell_command_with_multiple_env() {
        let mut env = BTreeMap::new();
        env.insert("RUST_LOG".into(), "debug".into());
        env.insert("CARGO_INCREMENTAL".into(), "0".into());
        let cmd = build_shell_command("cargo", &["build".into()], &env);
        // BTreeMap maintains sorted order by key
        assert_eq!(cmd, "CARGO_INCREMENTAL=0 RUST_LOG=debug cargo build");
    }

    #[test]
    fn test_build_shell_command_env_and_args() {
        let mut env = BTreeMap::new();
        env.insert("KEY".into(), "value".into());
        let args = vec!["test".into(), "--lib".into()];
        let cmd = build_shell_command("cargo", &args, &env);
        assert_eq!(cmd, "KEY=value cargo test --lib");
    }

    #[test]
    fn test_needs_nix_wrapper_for_nix_platform() {
        let shell = FloweyShell::new(FlowPlatform::Linux(FlowPlatformLinuxDistro::Nix)).unwrap();
        assert!(shell.needs_nix_wrapper());
    }

    #[test]
    fn test_needs_nix_wrapper_for_ubuntu() {
        let shell = FloweyShell::new(FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu)).unwrap();
        assert!(!shell.needs_nix_wrapper());
    }

    #[test]
    fn test_needs_nix_wrapper_for_windows() {
        let shell = FloweyShell::new(FlowPlatform::Windows).unwrap();
        assert!(!shell.needs_nix_wrapper());
    }

    #[test]
    fn test_deref_to_inner_shell() {
        let shell = FloweyShell::new(FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu)).unwrap();
        // Test that we can call xshell::Shell methods via Deref
        let _current_dir = shell.current_dir();
    }
}
