// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use crate::shell::XtaskShell;

/// Clean build artifacts beyond what `cargo clean` removes.
///
/// By default, removes: flowey-out, flowey-persist, .packages, and
/// vmm_test_results. Use `--cargo` to also clean the cargo build output.
#[derive(Debug, clap::Parser)]
#[clap(about = "Clean build system artifacts (flowey, packages, test results)")]
pub struct Clean {
    /// Clean everything (equivalent to --cargo)
    #[clap(long)]
    all: bool,

    /// Also run `cargo clean` to remove the target/ directory
    #[clap(long)]
    cargo: bool,

    /// Print what would be removed without actually deleting anything
    #[clap(long)]
    dry_run: bool,
}

/// Directories that are always cleaned.
const DEFAULT_DIRS: &[&str] = &[
    "flowey-out",
    "flowey-persist",
    ".packages",
    "vmm_test_results",
];

impl Xtask for Clean {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let do_cargo = self.all || self.cargo;

        let mut removed_anything = false;

        // Remove default directories
        for dir_name in DEFAULT_DIRS {
            let path = ctx.root.join(dir_name);
            if path.exists() {
                if self.dry_run {
                    println!("would remove {}", path.display());
                } else {
                    println!("removing {}", path.display());
                    fs_err::remove_dir_all(&path)?;
                }
                removed_anything = true;
            }
        }

        // Optionally run cargo clean
        if do_cargo {
            if self.dry_run {
                println!("would run `cargo clean`");
            } else {
                println!("running `cargo clean`");
                let sh = XtaskShell::new()?;
                sh.cmd("cargo").arg("clean").run()?;
            }
            removed_anything = true;
        }

        if !removed_anything {
            println!("nothing to clean");
        }

        Ok(())
    }
}
