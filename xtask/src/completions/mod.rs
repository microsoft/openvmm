// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use clap::Parser;
use std::io::IsTerminal;
use std::io::Write;

#[derive(Clone, clap::ValueEnum)]
enum Shell {
    Zsh,
}

/// Emit shell-completion script
#[derive(Parser)]
pub struct Completions {
    /// Supported shells
    shell: Shell,
}

impl Completions {
    pub fn run(self) -> anyhow::Result<()> {
        match self.shell {
            Shell::Zsh => {
                std::io::stdout().write_all(include_bytes!("./complete.zsh"))?;
                if std::io::stdout().is_terminal() {
                    eprintln!(
                        "{}",
                        ZSH_HELP.replace(
                            "<<CMD_PATH>>",
                            &std::env::current_exe()?.display().to_string()
                        )
                    );
                }
            }
        }

        Ok(())
    }
}

const ZSH_HELP: &str = r#"
# To enable `cargo xtask` completions, there are two steps:
#
# 1. Use `rustup completions cargo` to set up `cargo` completions.
# 2. Copy this script into your `.zshrc`
#
# NOTE: This is _not_ your typical `zsh` completion!
#
# No need to `compdef` anything. Just make sure that the `_cargo-xtask` function
# is in-scope, and that `rustup completions cargo` infrastructure redirect
# `cargo xtask` completions to that function.
"#;

pub(crate) struct XtaskCompleteFactory {
    pub ctx: crate::XtaskCtx,
}

impl clap_dyn_complete::CustomCompleterFactory for XtaskCompleteFactory {
    type CustomCompleter = XtaskComplete;
    async fn build(&self, _ctx: &clap_dyn_complete::RootCtx<'_>) -> Self::CustomCompleter {
        XtaskComplete {
            ctx: self.ctx.clone(),
        }
    }
}

pub(crate) struct XtaskComplete {
    ctx: crate::XtaskCtx,
}

impl clap_dyn_complete::CustomCompleter for XtaskComplete {
    async fn complete(
        &self,
        _ctx: &clap_dyn_complete::RootCtx<'_>,
        command_path: &[&str],
        arg_id: &str,
    ) -> Vec<String> {
        match (command_path, arg_id) {
            (["xtask", "fuzz", cmd], "target")
                if matches!(
                    *cmd,
                    "run"
                        | "build"
                        | "clean"
                        | "fmt"
                        | "cmin"
                        | "tmin"
                        | "coverage"
                        | "onefuzz-allowlist"
                ) =>
            {
                crate::tasks::cli_completions::fuzz::complete_fuzzer_targets(&self.ctx)
            }
            _ => Vec::new(),
        }
    }
}
