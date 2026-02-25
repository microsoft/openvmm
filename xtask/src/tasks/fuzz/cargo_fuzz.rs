// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Glue to invoke external `cargo-fuzz` commands

use crate::shell::XtaskShell;
use anyhow::Context;
use std::path::Path;
use std::path::PathBuf;

pub(super) enum CargoFuzzCommand {
    Build,
    Run { artifact: Option<PathBuf> },
    Fmt { input: PathBuf },
    Cmin,
    Tmin { test_case: PathBuf },
    Coverage,
}

impl CargoFuzzCommand {
    fn to_args<'a, 'b: 'a>(&'b self, target: &'a str) -> Vec<&'a str> {
        match self {
            CargoFuzzCommand::Build => {
                vec!["build", target]
            }
            CargoFuzzCommand::Run { artifact } => {
                let mut args = vec!["run", target];
                if let Some(artifact) = artifact {
                    args.push(artifact.to_str().unwrap())
                }
                args
            }
            CargoFuzzCommand::Fmt { input } => {
                vec!["fmt", target, input.to_str().unwrap()]
            }
            CargoFuzzCommand::Cmin => {
                vec!["cmin", target]
            }
            CargoFuzzCommand::Tmin { test_case } => {
                vec!["tmin", target, test_case.to_str().unwrap()]
            }
            CargoFuzzCommand::Coverage => {
                vec!["coverage", target]
            }
        }
    }

    pub(super) fn invoke(
        self,
        target_name: &str,
        fuzz_dir: &Path,
        target_options: &[String],
        toolchain: Option<&str>,
        extra: &[String],
    ) -> anyhow::Result<()> {
        if which::which("cargo-fuzz").is_err() {
            anyhow::bail!("could not find cargo-fuzz! did you run `cargo install cargo-fuzz`?");
        }

        let sh = XtaskShell::new()?;
        if matches!(&self, CargoFuzzCommand::Run { artifact: Some(_) }) {
            sh.set_var("XTASK_FUZZ_REPRO", "1");
        }

        let mut toolchain_check_cmd = sh.cmd("rustc");
        if let Some(toolchain_override) = toolchain {
            toolchain_check_cmd = toolchain_check_cmd.arg(format!("+{}", toolchain_override));
        }
        let result = toolchain_check_cmd
            .arg("-V")
            .output()
            .context("could not detect toolchain! did you run `rustup toolchain install`?")?;
        let output = std::str::from_utf8(&result.stdout)?.to_ascii_lowercase();
        let is_nightly = output.contains("-nightly") || output.contains("-dev");

        let mut cmd = sh.cmd("cargo");
        if let Some(toolchain_override) = toolchain {
            cmd = cmd.arg(format!("+{}", toolchain_override));
        }
        cmd = cmd.arg("fuzz");
        cmd = cmd.args(self.to_args(target_name));
        cmd = cmd.arg("--fuzz-dir").arg(fuzz_dir);

        if is_nightly {
            // Sanitizers can be enabled, leave defaults alone
        } else if std::env::var_os("CARGO").is_some() {
            // We are running in a stable toolchain `cargo xtask` invocation.
            // Cargo prevents us from setting RUSTC_BOOTSTRAP for a nested
            // invocation, so we can't enable sanitizers.
            log::warn!(
                "Running on a stable toolchain in a `cargo xtask` invocation, disabling sanitizers"
            );
            log::warn!(
                "To enable sanitizers, run {} directly, or switch to a nightly toolchain",
                std::env::current_exe()?.display()
            );
            cmd = cmd.args(["-s", "none"]);
        } else {
            // Non-cargo invocation, sanitizers can be enabled via RUSTC_BOOTSTRAP
            log::warn!("Running on a stable toolchain, enabling sanitizers via RUSTC_BOOTSTRAP");
            cmd = cmd.env("RUSTC_BOOTSTRAP", "1");
        }

        cmd = cmd.args(extra);
        if self.supports_target_options() && !target_options.is_empty() {
            if !extra.iter().any(|x| x == "--") {
                cmd = cmd.arg("--");
            }
            cmd = cmd.args(target_options);
        }

        cmd.run()?;

        Ok(())
    }

    fn supports_target_options(&self) -> bool {
        matches!(self, CargoFuzzCommand::Run { .. })
    }
}
