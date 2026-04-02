// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::FmtPass;
use crate::fs_helpers::git_diffed;
use crate::shell::XtaskShell;
use crate::tasks::fmt::FmtCtx;

pub struct Rustfmt;

impl FmtPass for Rustfmt {
    fn run(self, ctx: FmtCtx) -> anyhow::Result<()> {
        let FmtCtx {
            ctx,
            fix,
            only_diffed,
        } = ctx;
        let sh = XtaskShell::new()?;
        let rust_toolchain = sh.var("RUST_TOOLCHAIN").map(|s| format!("+{s}")).ok();
        let fmt_check = (!fix).then_some("--check");

        if only_diffed {
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
        } else {
            sh.cmd("cargo")
                .args(rust_toolchain)
                .args(["fmt", "--"])
                .args(fmt_check)
                .quiet()
                .run()?;
        }

        Ok(())
    }
}
