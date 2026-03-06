// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use crate::shell::XtaskShell;
use anyhow::Context;
use clap::Parser;

#[derive(Parser)]
#[clap(about = "Ensure all flowey pipelines are up to date")]
pub struct VerifyFlowey {
    /// Fix any out-of-date pipelines
    #[clap(long)]
    pub fix: bool,
}

impl VerifyFlowey {
    pub fn new(fix: bool) -> Self {
        Self { fix }
    }
}

impl Xtask for VerifyFlowey {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        // need to go through all this rigamarole because `cargo --quiet
        // xflowey regen` doesn't do what you'd hope it'd do
        let cmd = {
            let data = fs_err::read_to_string(ctx.root.join(".cargo/config.toml"))?;
            let mut cmd = None;
            for ln in data.lines() {
                if let Some(ln) = ln.trim().strip_prefix(r#"xflowey = ""#) {
                    let alias = ln
                        .strip_suffix('"')
                        .context("invalid .cargo/config.toml")?
                        .split(' ')
                        .map(|s| s.to_owned())
                        .collect::<Vec<_>>();
                    cmd = Some(alias);
                }
            }
            cmd.context("could not find `xflowey` alias in .cargo/config.toml")?
        };

        let check = (!self.fix).then_some("--check");

        let sh = XtaskShell::new()?;
        sh.cmd("cargo")
            .arg("--quiet")
            .args(&cmd)
            .args(["regen", "--quiet"])
            .args(check)
            .quiet()
            .run()?;

        Ok(())
    }
}
