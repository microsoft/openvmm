// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod lints;
mod rustfmt;
mod verify_flowey;

use crate::Xtask;
use anyhow::Context;
use clap::Parser;
use heck::ToKebabCase;

/// Xtask to run various repo-specific formatting checks
#[derive(Parser)]
#[clap(about = "Run various formatting checks")]
pub struct Fmt {
    /// Attempt to fix any formatting issues
    ///
    /// NOTE: setting this flag disables pass-level parallelism
    #[clap(long)]
    fix: bool,

    /// Don't run passes in parallel (avoiding potentially interweaved output)
    #[clap(long)]
    no_parallel: bool,

    /// Only run checks on files that are currently diffed
    #[clap(long)]
    only_diffed: bool,

    /// Run only certain formatting passes
    #[clap(long)]
    pass: Vec<PassName>,
}

/// Common trait implemented by all Fmt passes.
pub trait FmtPass {
    /// Run the pass.
    ///
    /// For consistency and simplicity, `FmtPass` implementations are allowed to
    /// assume that they are being run from the root of the repo's filesystem.
    fn run(self, ctx: FmtCtx) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub struct FmtCtx {
    ctx: crate::XtaskCtx,
    fix: bool,
    only_diffed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
enum PassName {
    // Keep Rustfmt first since some lints may depend on proper formatting
    Rustfmt,
    Lints,
    VerifyFuzzers,
    VerifyFlowey,
}

impl Xtask for Fmt {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let tasks: Vec<Box<dyn FnOnce() -> anyhow::Result<()> + Send>> = {
            fn wrapper(
                ctx: &FmtCtx,
                name: String,
                func: impl FnOnce(FmtCtx) -> anyhow::Result<()> + Send + 'static,
            ) -> Box<dyn FnOnce() -> anyhow::Result<()> + Send> {
                let ctx = ctx.clone();

                Box::new(move || {
                    let start_time = std::time::Instant::now();
                    log::info!("[checking] {}", name);
                    let res = func(ctx).context(format!("while running {name}"));
                    log::info!(
                        "[complete] {} ({:.2?})",
                        name,
                        std::time::Instant::now() - start_time
                    );
                    res
                })
            }

            let passes = if !self.pass.is_empty() {
                let mut passes = self.pass.clone();
                passes.sort();
                passes.dedup_by(|a, b| a == b);
                passes
            } else {
                // Run all of them by default.
                // Run rustfmt first since lints may depend on proper formatting
                vec![
                    PassName::Rustfmt,
                    PassName::Lints,
                    PassName::VerifyFuzzers,
                    PassName::VerifyFlowey,
                ]
            };

            let ctx = FmtCtx {
                ctx,
                fix: self.fix,
                only_diffed: self.only_diffed,
            };

            passes
                .into_iter()
                .map(|pass| {
                    let name = format!("{:?}", pass).to_kebab_case();
                    match pass {
                        PassName::Rustfmt => {
                            wrapper(&ctx, name, move |ctx| rustfmt::Rustfmt.run(ctx))
                        }
                        PassName::Lints => wrapper(&ctx, name, move |ctx| lints::Lints.run(ctx)),
                        PassName::VerifyFuzzers => wrapper(&ctx, name, {
                            move |ctx| crate::tasks::fuzz::VerifyFuzzers.run(ctx.ctx)
                        }),
                        PassName::VerifyFlowey => wrapper(&ctx, name, {
                            move |ctx| verify_flowey::VerifyFlowey.run(ctx)
                        }),
                    }
                })
                .collect()
        };

        let results: Vec<_> = if self.fix || self.no_parallel {
            tasks.into_iter().map(|f| (f)()).collect()
        } else {
            tasks
                .into_iter()
                .map(std::thread::spawn)
                .collect::<Vec<_>>()
                .into_iter()
                .map(|j| j.join().unwrap())
                .collect()
        };

        for res in results.iter() {
            if let Err(e) = res {
                log::error!("{:#}", e);
            }
        }

        if results.iter().any(|res| res.is_err()) && !self.fix {
            log::error!(
                "run `cargo xtask fmt{}{} --fix`",
                if self.only_diffed {
                    " --only-diffed"
                } else {
                    ""
                },
                if !self.pass.is_empty() {
                    self.pass
                        .into_iter()
                        .map(|pass| format!(" --pass {}", format!("{:?}", pass).to_kebab_case()))
                        .collect::<Vec<_>>()
                        .join("")
                } else {
                    "".into()
                }
            );
            Err(anyhow::anyhow!("found formatting errors"))
        } else {
            Ok(())
        }
    }
}
