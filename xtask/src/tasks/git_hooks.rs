// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use anyhow::Context;
use clap::Parser;
use clap::ValueEnum;
use serde::Deserialize;
use serde::Serialize;
use std::io::BufRead;
use std::path::Path;

/// Xtask to install git hooks back into `xtask`.
///
/// Must be installed alongside [`RunGitHook`] as a top-level `hook` subcommand.
#[derive(Parser)]
#[clap(
    about = "Install git pre-commit / pre-push hooks",
    disable_help_subcommand = true
)]
pub struct InstallGitHooks {
    /// Install the pre-commit hook (only runs quick checks)
    #[clap(long)]
    pre_commit: bool,

    /// Install the pre-push hook
    #[clap(long)]
    pre_push: bool,

    /// Run formatting checks as part of the hook
    #[clap(long, default_value = "yes")]
    with_fmt: YesNo,
}

#[derive(Clone, ValueEnum)]
enum YesNo {
    Yes,
    No,
}

const CONFIG_HEREDOC: &str = "XTASK_HOOK_CONFIG";

// This bit of bash script is the "minimum-viable-glue" required to do 2 things:
//
// 1. Invoke `cargo xtask hook <hook-kind>`.
// 2. Encode the CONFIG blob that gets passed to the xtask (which contains
//    user-customizable hook configuration, generated based on what args were
//    passed to `install-git-hooks`)
const TEMPLATE: &str = r#"
#!/bin/sh

set -e

###############################################################################
#          ANY MODIFICATIONS MADE TO THIS FILE WILL GET OVERWRITTEN!          #
###############################################################################

# This file is generated (and re-generated) by `cargo xtask`.
#
# To opt-out of automatic updates, it is sufficient to delete the following
# CONFIG variable, and `cargo xtask` will no longer overwrite this file.

CONFIG=$(cat << <<CONFIG_HEREDOC>>
<<CONFIG>>
<<CONFIG_HEREDOC>>
)

# The rest of the script is the "minimum-viable-bash" required to do 2 things:
#
# 1. Invoke `cargo xtask hook <hook-kind>`.
# 2. Encode the $CONFIG blob that gets passed to the xtask, which contains the
#    user-specified hook configuration (as specified via `install-git-hooks`)
#
# Any future additions to `xtask`-driven hooks should be done in Rust (as
# opposed to extending this bash script)

cd "${GIT_DIR-$(git rev-parse --git-dir)}/.."

XTASK="cargo xtask"

USE_PREBUILT_XTASK="<<USE_PREBUILT_XTASK>>"
if [ -n "$USE_PREBUILT_XTASK" ] && [ -f "<<XTASK_PATH_FILE>>" ]; then
    XTASK=$(cat "<<XTASK_PATH_FILE>>")
fi

$XTASK hook <<HOOK_KIND>> $CONFIG

"#;

fn install_hook(
    root: &Path,
    config: HookConfig,
    kind: &str,
    rebuild: bool,
    quiet: bool,
) -> anyhow::Result<()> {
    let script = TEMPLATE;
    let script = script.replace("<<CONFIG_HEREDOC>>", CONFIG_HEREDOC);
    let script = script.replace("<<CONFIG>>", &serde_json::to_string(&config)?);
    let script = script.replace("<<USE_PREBUILT_XTASK>>", if !rebuild { "1" } else { "" });
    let script = script.replace("<<XTASK_PATH_FILE>>", crate::XTASK_PATH_FILE);
    let script = script.replace("<<HOOK_KIND>>", kind);
    let script = script.trim();

    let path = root.join(".git").join("hooks").join(kind);
    let already_exists = path.exists();

    fs_err::write(&path, script)?;

    // enable exec on unix systems
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs_err::metadata(&path)?.permissions();
        perms.set_mode(perms.mode() | 0o100);
        fs_err::set_permissions(&path, perms)?;
    }

    let lvl = {
        if quiet {
            log::Level::Debug
        } else {
            log::Level::Info
        }
    };

    if already_exists {
        log::log!(lvl, "updated {}", path.display());
    } else {
        log::log!(lvl, "installed {}", path.display());
    }

    Ok(())
}

fn install_pre_commit(root: &Path, config: HookConfig, quiet: bool) -> anyhow::Result<()> {
    install_hook(root, config, "pre-commit", false, quiet)
}

fn install_pre_push(root: &Path, config: HookConfig, quiet: bool) -> anyhow::Result<()> {
    install_hook(root, config, "pre-push", true, quiet)
}

impl Xtask for InstallGitHooks {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        if ![self.pre_commit, self.pre_push].into_iter().any(|x| x) {
            log::warn!("no hooks installed! pass at least one of [--pre-commit, --pre-push]")
        }

        if self.pre_commit {
            install_pre_commit(
                &ctx.root,
                HookConfig {
                    with_fmt: matches!(self.with_fmt, YesNo::Yes),
                },
                false,
            )?;
        }

        if self.pre_push {
            install_pre_push(
                &ctx.root,
                HookConfig {
                    with_fmt: matches!(self.with_fmt, YesNo::Yes),
                },
                false,
            )?;
        }

        Ok(())
    }
}

#[derive(Default, Serialize, Deserialize)]
struct HookConfig {
    with_fmt: bool,
}

#[derive(Debug)]
enum HookError {
    Missing,
    Custom,
    MalformedConfig,
}

fn extract_config(path: &Path) -> Result<HookConfig, HookError> {
    let f = fs_err::File::open(path).map_err(|_| HookError::Missing)?;
    let f = std::io::BufReader::new(f);
    let mut found_config = false;
    for ln in f.lines() {
        // is a line isn't UTF-8, assume this is a custom hook
        let ln = ln.map_err(|_| HookError::Custom)?;

        if !found_config {
            if ln.ends_with(CONFIG_HEREDOC) {
                found_config = true;
            }
            continue;
        }

        return serde_json::from_str(&ln).map_err(|_| HookError::MalformedConfig);
    }

    // if we couldn't find the config, assume this is a custom git hook
    Err(HookError::Custom)
}

/// Keeps any installed hooks up to date.
pub fn update_hooks(root: &Path) -> anyhow::Result<()> {
    let base_path = root.join(".git").join("hooks");

    let update_hook_inner =
        |hook: &str,
         install_fn: fn(root: &Path, config: HookConfig, quiet: bool) -> anyhow::Result<()>,
         quiet: bool|
         -> anyhow::Result<()> {
            match extract_config(&base_path.join(hook)) {
                Ok(config) => (install_fn)(root, config, quiet)?,
                Err(HookError::MalformedConfig) => {
                    log::warn!("detected malformed {hook} hook!");
                    log::warn!("please rerun `cargo xtask install-git-hooks --{hook}`!");
                }
                Err(e) => {
                    log::debug!("could not update {hook} hook: {:?}", e)
                }
            }

            Ok(())
        };

    update_hook_inner("pre-commit", install_pre_commit, true)?;
    update_hook_inner("pre-push", install_pre_push, true)?;

    Ok(())
}

/// Private subcommand to run hooks (invoked via `git`).
///
/// This subcommand should be marked as `#[clap(hide = true)]`, as it shouldn't
/// be invoked by end-users. It is an internal implementation detail of the
/// `xtask` git hook infrastructure.
#[derive(Parser)]
pub struct RunGitHook {
    hook: HookVariety,
    config: String,
}

#[derive(Clone, ValueEnum)]
enum HookVariety {
    PreCommit,
    PrePush,
}

impl Xtask for RunGitHook {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let config: HookConfig =
            serde_json::from_str(&self.config).context("invalid hook config")?;

        match self.hook {
            // pre-commit should only do quick checks on modified files
            HookVariety::PreCommit => {
                log::info!("running pre-commit hook");

                if config.with_fmt {
                    const FMT_CMD: &str = "fmt --only-diffed --pass rustfmt --pass house-rules";
                    crate::tasks::Fmt::parse_from(FMT_CMD.split(' ')).run(ctx)?;
                }
            }
            // pre-push should do all "heavier" checks
            HookVariety::PrePush => {
                log::info!("running pre-push hook");

                if config.with_fmt {
                    const FMT_CMD: &str = "";
                    crate::tasks::Fmt::parse_from(FMT_CMD.split(' ')).run(ctx)?;
                }
            }
        }

        log::info!("hook completed successfully\n");

        Ok(())
    }
}
