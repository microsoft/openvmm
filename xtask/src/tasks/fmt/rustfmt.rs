// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use crate::fs_helpers::git_diffed;
use crate::shell::XtaskShell;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[clap(about = "Check that all repo files are formatted using rustfmt")]
pub struct Rustfmt {
    /// Run `rustfmt` on all `.rs` files in the repo
    #[clap(long)]
    pub fix: bool,

    /// A list of files to check
    ///
    /// If no files were provided, all files in-tree will be checked
    pub files: Vec<PathBuf>,

    /// Only run checks on files that are currently diffed
    #[clap(long, conflicts_with = "files")]
    pub only_diffed: bool,
}

impl Rustfmt {
    pub fn new(fix: bool, only_diffed: bool) -> Self {
        Self {
            fix,
            files: Vec::new(),
            only_diffed,
        }
    }
}

#[derive(Debug)]
enum Files {
    All,
    OnlyDiffed,
    Specific(Vec<PathBuf>),
}

impl Xtask for Rustfmt {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let files = if self.only_diffed {
            Files::OnlyDiffed
        } else if self.files.is_empty() {
            Files::All
        } else {
            Files::Specific(self.files)
        };

        log::trace!("running rustfmt on {:?}", files);

        let sh = XtaskShell::new()?;
        let rust_toolchain = sh.var("RUST_TOOLCHAIN").map(|s| format!("+{s}")).ok();
        let fmt_check = (!self.fix).then_some("--check");

        match files {
            Files::All => {
                sh.cmd("cargo")
                    .args(rust_toolchain)
                    .args(["fmt", "--"])
                    .args(fmt_check)
                    .quiet()
                    .run()?;
            }
            Files::OnlyDiffed => {
                let mut files = git_diffed(ctx.in_git_hook)?;
                files.retain(|f| f.extension().unwrap_or_default() == "rs");

                if !files.is_empty() {
                    let res = sh
                        .cmd("rustfmt")
                        .args(rust_toolchain)
                        .args(fmt_check)
                        .args(&files)
                        .quiet()
                        .run();

                    if res.is_err() {
                        anyhow::bail!("found formatting issues in diffed files");
                    }
                }
            }
            Files::Specific(files) => {
                assert!(!files.is_empty());

                sh.cmd("rustfmt")
                    .args(rust_toolchain)
                    .args(fmt_check)
                    .args(&files)
                    .quiet()
                    .run()?;
            }
        }

        log::trace!("done rustfmt");
        Ok(())
    }
}
