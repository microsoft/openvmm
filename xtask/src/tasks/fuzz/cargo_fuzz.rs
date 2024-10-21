// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Glue to invoke external `cargo-fuzz` commands

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
        let sh = xshell::Shell::new()?;

        if matches!(&self, CargoFuzzCommand::Run { artifact: Some(_) }) {
            sh.set_var("XTASK_FUZZ_REPRO", "1");
        }

        let toolchain = toolchain.unwrap_or("nightly");
        let cmd_args = self.to_args(target_name);

        let mut trailing_args = Vec::new();

        if !extra.is_empty() {
            trailing_args.extend_from_slice(extra);
        }

        if self.supports_target_options() && !target_options.is_empty() {
            if trailing_args.is_empty() {
                trailing_args.push("--".into());
            }
            trailing_args.extend_from_slice(target_options);
        }

        let cmd = if toolchain != "default" {
            let toolchain_arg = format!("+{}", toolchain);

            if xshell::cmd!(sh, "cargo {toolchain_arg}")
                .quiet()
                .ignore_stderr()
                .ignore_stdout()
                .run()
                .is_err()
            {
                anyhow::bail!("could not detect {toolchain} toolchain! did you run `rustup toolchain install {toolchain}`?");
            }
            xshell::cmd!(
                sh,
                "cargo {toolchain_arg} fuzz {cmd_args...} --fuzz-dir {fuzz_dir} {trailing_args...}"
            )
        } else {
            xshell::cmd!(
                sh,
                "cargo fuzz {cmd_args...} --fuzz-dir {fuzz_dir} {trailing_args...}"
            )
        };

        if which::which("cargo-fuzz").is_err() {
            anyhow::bail!("could not find cargo-fuzz! did you run `cargo install cargo-fuzz`?");
        }

        cmd.run()?;

        Ok(())
    }

    fn supports_target_options(&self) -> bool {
        matches!(self, CargoFuzzCommand::Run { .. })
    }
}
