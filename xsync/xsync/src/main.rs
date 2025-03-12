// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tooling to sync dependencies in OpenVMM "overlay" repos with OpenVMM.

use anyhow::Context;
use clap::Parser;
use clap::Subcommand;
use std::path::PathBuf;

mod tasks;

/// Default location to maintain a `xsync-path` file
///
/// This file contains a fully-resolved path to the actual `xsync` binary,
/// allowing other tooling (e.g: git hook) to invoke the xsync without having
/// to go through `cargo`.
const XSYNC_PATH_FILE: &str = "./target/xsync-path";

/// Common context passed into every Cmd
#[derive(Clone)]
pub struct CmdCtx {
    /// Whether --check was passed at the top-level
    pub check: bool,
    /// Path to the overlay workspace (i.e: the one that will get updated)
    pub overlay_workspace: PathBuf,
    /// Path to the base workspace (i.e: the one that the overlay will sync to)
    pub base_workspace: PathBuf,
}

/// Common trait implemented by all Cmd subcommands.
trait Cmd: clap::Parser {
    /// Run the Cmd.
    fn run(self, ctx: CmdCtx) -> anyhow::Result<()>;
}

#[derive(Parser)]
#[clap(name = "xsync")]
struct Cli {
    /// Path to the base workspace (i.e: the one that the overlay will sync to)
    overlay_workspace: PathBuf,

    /// Path to the overlay workspace (i.e: the one that will get updated)
    base_workspace: PathBuf,

    /// Check that all checked-in files are up-to-date.
    #[clap(long)]
    check: bool,

    #[clap(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    CargoToml(tasks::CargoToml),
    CargoLock(tasks::CargoLock),
    RustToolchainToml(tasks::RustToolchainToml),
    RustfmtToml(tasks::RustfmtToml),
}

fn main() {
    ci_logger::init("XSYNC_LOG").unwrap();

    if let Err(e) = try_main() {
        log::error!("Error: {:#}", e);
        std::process::exit(-1);
    }
}

fn try_main() -> anyhow::Result<()> {
    let Cli {
        overlay_workspace,
        base_workspace,
        check,
        command,
    } = Cli::parse();

    let overlay_workspace =
        dunce::canonicalize(overlay_workspace).context("invalid overlay_workspace!")?;
    let base_workspace = dunce::canonicalize(base_workspace).context("invalid base_workspace!")?;

    // drop the path to the xsync binary in an easy-to-find place. this gets
    // used by pre-commit hooks to avoid rebuilding the xsync.
    if let Ok(path) = std::env::current_exe() {
        if let Err(e) = fs_err::write(XSYNC_PATH_FILE, path.display().to_string()) {
            log::debug!("Unable to create XSYNC_PATH_FILE: {:#}", e)
        }
    }

    let ctx = CmdCtx {
        check,
        overlay_workspace: overlay_workspace.clone(),
        base_workspace: base_workspace.clone(),
    };

    let res = match command {
        Some(cmd) => match cmd {
            Commands::CargoToml(task) => task.run(ctx),
            Commands::CargoLock(task) => task.run(ctx),
            Commands::RustToolchainToml(task) => task.run(ctx),
            Commands::RustfmtToml(task) => task.run(ctx),
        },
        None => do_full_sync(&ctx, check),
    };

    if res.is_err() && std::env::var("SKIP_FIX_TEXT").is_err() {
        log::warn!("To fix:");
        log::warn!("  - Change dirs so you're inside the `xsync` folder");
        log::warn!(
            "  - Run the following command: cargo run -- {} {}",
            overlay_workspace.display(),
            base_workspace.display()
        );
    }

    res
}

fn do_full_sync(ctx: &CmdCtx, check: bool) -> Result<(), anyhow::Error> {
    log::info!(
        "running xsync cmd: `rust-toolchain regen`    (syncing overlay-repo's `rust-toolchain.toml` to base-repo's `rust-toolchain.toml`)"
    );
    tasks::RustToolchainToml {
        cmd: tasks::rust_toolchain_toml::Command::Regen,
    }
    .run(ctx.clone())?;

    log::info!(
        "running xsync cmd: `rustfmt regen`    (syncing overlay-repo's `rustfmt.toml` to base-repo's `rustfmt.toml`)"
    );
    tasks::RustfmtToml {
        cmd: tasks::rustfmt_toml::Command::Regen,
    }
    .run(ctx.clone())?;

    log::info!(
        "running xsync cmd: `cargo-toml regen`    (regenerating overlay-repo `Cargo.toml` using `Cargo.xsync.toml`)"
    );
    tasks::CargoToml {
        cmd: tasks::cargo_toml::Command::Regen,
    }
    .run(ctx.clone())?;

    if !check {
        log::info!(
            "running: `cargo update --workspace` in {}    (ensuring base-repo `Cargo.lock` is up-to-date)",
            ctx.base_workspace.display()
        );
        let status = std::process::Command::new("cargo")
            .arg("update")
            .arg("--workspace")
            .arg("--quiet")
            .current_dir(&ctx.base_workspace)
            .status()?;
        if !status.success() {
            return Err(anyhow::anyhow!(
                "cargo update failed with status: {}",
                status
            ));
        }
    }

    log::info!(
        "running xsync cmd: `cargo-lock gen-external base`    (regenerating list of base-repo external dependencies)"
    );
    tasks::CargoLock {
        cmd: tasks::cargo_lock::Command::GenExternal {
            which: tasks::cargo_lock::Generate::Base,
        },
    }
    .run(ctx.clone())?;

    log::info!(
        "running xsync cmd: `cargo-lock gen-external overlay`    (regenerating list of overlay-repo external dependencies)"
    );
    tasks::CargoLock {
        cmd: tasks::cargo_lock::Command::GenExternal {
            which: tasks::cargo_lock::Generate::Overlay,
        },
    }
    .run(ctx.clone())?;

    log::info!(
        "running xsync cmd: `cargo-lock regen`    (syncing overlay-repo's `Cargo.lock` to base-repo's `Cargo.lock`)"
    );
    tasks::CargoLock {
        cmd: tasks::cargo_lock::Command::Regen,
    }
    .run(ctx.clone())?;

    log::info!(
        "running xsync cmd: `cargo-lock gen-external overlay`    (regenerating list of overlay-repo external dependencies (post-sync))"
    );
    tasks::CargoLock {
        cmd: tasks::cargo_lock::Command::GenExternal {
            which: tasks::cargo_lock::Generate::Overlay,
        },
    }
    .run(ctx.clone())?;

    Ok(())
}
