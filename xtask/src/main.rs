// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM repo-specific automation.
//!
//! If you're thinking of writing a bash script, write an xtask instead!
//!
//! Follows the xtask workflow/convention, as described at
//! <https://github.com/matklad/cargo-xtask>

#![forbid(unsafe_code)]

use anyhow::Context;
use clap::Parser;
use clap::Subcommand;
use std::path::Path;
use std::path::PathBuf;

mod completions;
pub mod fs_helpers;
pub mod shell;
pub mod tasks;

/// Default location to maintain a `xtask-path` file
///
/// This file contains a fully-resolved path to the actual `xtask` binary,
/// allowing other tooling (e.g: git hook) to invoke the xtask without having to
/// go through `cargo`.
pub const XTASK_PATH_FILE: &str = "./target/xtask-path";

/// Common context passed into every Xtask
#[derive(Clone)]
pub struct XtaskCtx {
    /// Project root directory
    pub root: PathBuf,
    /// xtask is running within a hook
    pub in_git_hook: bool,
    /// xtask was invoked as part of a "run on save" operation
    pub in_run_on_save: bool,
}

/// Common trait implemented by all Xtask subcommands.
pub trait Xtask: Parser {
    /// Run the Xtask.
    ///
    /// For consistency and simplicity, `Xtask` implementations are allowed to
    /// assume that they are being run from the root of the repo's filesystem.
    /// Callers of `Xtask::run` should take care to ensure
    /// [`std::env::set_current_dir`] was called prior to invoking `Xtask::run`.
    fn run(self, ctx: XtaskCtx) -> anyhow::Result<()>;
}

#[derive(Parser)]
#[clap(name = "xtask", about = "OpenVMM repo automation")]
struct Cli {
    #[clap(subcommand)]
    command: Commands,

    /// Specify a custom project root directory.
    ///
    /// Can be used to ensure consistent formatting between the OpenVMM base
    /// repo, and any custom out-of-tree overlay repos.
    #[clap(long)]
    custom_root: Option<PathBuf>,

    /// Signal that this `xtask` is being invoked as part of a "run on save"
    /// operation.
    ///
    /// When set, certain tasks may choose to skip certain expensive checks in
    /// order to execute as quickly as possible.
    ///
    /// e.g: instead of calling `cargo run` in order to execute a project-local
    /// tool, `xtask` may instead attempt to find and use an existing pre-built
    /// binary, if one is available. This will be faster, but may run the risk
    /// of executing a slightly stale binary.
    #[clap(long)]
    run_on_save: bool,
}

#[derive(Subcommand)]
enum Commands {
    #[clap(hide = true)]
    Hook(tasks::RunGitHook),
    #[clap(hide = true)]
    Complete(clap_dyn_complete::Complete),
    Completions(completions::Completions),

    Clean(tasks::Clean),
    Fmt(tasks::Fmt),
    Fuzz(tasks::Fuzz),
    GuestTest(tasks::GuestTest),
    InstallGitHooks(tasks::InstallGitHooks),
    VerifySize(tasks::VerifySize),
}

fn main() {
    ci_logger::init("XTASK_LOG").unwrap();

    if let Err(e) = try_main() {
        log::error!("Error: {:#}", e);
        std::process::exit(-1);
    }
}

fn try_main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let orig_root = Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .unwrap()
        .to_path_buf();

    let root = cli
        .custom_root
        .map(std::path::absolute)
        .transpose()?
        .unwrap_or(orig_root.clone());

    // for consistency, always run xtasks as though they were run from the root
    std::env::set_current_dir(&root)?;

    // drop the path to the xtask binary in an easy-to-find place. this gets
    // used by the pre-commit hook, as well as the fmt-on-save dev-flow to avoid
    // rebuilding the xtask.
    if let Ok(path) = std::env::current_exe() {
        if let Err(e) = fs_err::write(orig_root.join(XTASK_PATH_FILE), path.display().to_string()) {
            log::debug!("Unable to create XTASK_PATH_FILE: {:#}", e)
        }
    }

    if !matches!(cli.command, Commands::Complete(..)) {
        tasks::update_hooks(&root).context("failed to update git hooks")?;
    }

    let ctx = XtaskCtx {
        root,
        in_git_hook: matches!(cli.command, Commands::Hook(..)),
        in_run_on_save: cli.run_on_save,
    };

    match cli.command {
        Commands::Hook(task) => task.run(ctx),
        Commands::Completions(task) => task.run(),
        Commands::Complete(task) => {
            futures::executor::block_on(task.println_to_stub_script::<Cli>(
                Some("cargo"),
                completions::XtaskCompleteFactory { ctx },
            ));
            Ok(())
        }

        Commands::Clean(task) => task.run(ctx),
        Commands::Fmt(task) => task.run(ctx),
        Commands::Fuzz(task) => task.run(ctx),
        Commands::GuestTest(task) => task.run(ctx),
        Commands::InstallGitHooks(task) => task.run(ctx),
        Commands::VerifySize(task) => task.run(ctx),
    }
}
