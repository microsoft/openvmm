// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use clap::Parser;
use clap::CommandFactory;
use std::io::IsTerminal;

#[derive(Clone, clap::ValueEnum)]
enum Shell {
    /// [Fish](https://fishshell.com/)
    Fish,
    /// [Powershell](https://docs.microsoft.com/en-us/powershell/)  
    Powershell,
    /// [Zsh](https://www.zsh.org/)
    Zsh,
}

/// Emit shell-completion script
#[derive(Parser)]
pub struct Completions {
    /// Supported shells
    shell: Shell,
}

/// Generate static completions using clap_complete
#[derive(Parser)]
pub struct GenerateCompletions {
    /// Shell to generate completions for
    shell: clap_complete::Shell,
}

impl Completions {
    pub fn run(self) -> anyhow::Result<()> {
        let shell = match self.shell {
            Shell::Fish => clap_dyn_complete::Shell::Fish,
            Shell::Powershell => clap_dyn_complete::Shell::Powershell,
            Shell::Zsh => clap_dyn_complete::Shell::Zsh,
        };

        clap_dyn_complete::emit_completion_stub(
            shell,
            "xtask",
            "complete",
            &mut std::io::stdout(),
        )?;

        if IsTerminal::is_terminal(&std::io::stdout()) {
            eprintln!(
                "{}",
                match self.shell {
                    Shell::Fish => FISH_HELP,
                    Shell::Powershell => POWERSHELL_HELP,
                    Shell::Zsh => ZSH_HELP,
                }.replace(
                    "<<CMD_PATH>>",
                    &std::env::current_exe()?.display().to_string()
                )
            );
        }

        Ok(())
    }
}

impl GenerateCompletions {
    pub fn run(self) -> anyhow::Result<()> {
        let mut cmd = crate::Cli::command();
        clap_complete::generate(
            self.shell,
            &mut cmd,
            "xtask",
            &mut std::io::stdout(),
        );
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

const FISH_HELP: &str = r#"
# To enable `cargo xtask` completions for Fish:
#
# 1. Use `rustup completions fish cargo` to set up `cargo` completions.
# 2. Save this script to ~/.config/fish/completions/cargo-xtask.fish
"#;

const POWERSHELL_HELP: &str = r#"
# To enable `cargo xtask` completions for PowerShell:
#
# 1. Use `rustup completions powershell cargo` to set up `cargo` completions.
# 2. Add this script to your PowerShell profile
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
