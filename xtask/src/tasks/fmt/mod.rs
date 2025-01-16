// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod house_rules;
mod rustfmt;
mod unused_deps;
mod verify_flowey;
mod workspace;

use crate::Xtask;
use anyhow::Context;
use clap::Parser;

/// Xtask to run various repo-specific formatting checks
#[derive(Parser)]
#[clap(
    about = "Run various formatting checks",
    disable_help_subcommand = true,
    subcommand_value_name = "PASS",
    subcommand_help_heading = "PASSES",
    after_help = r#"NOTES:

    For documentation on how each pass works, see the corresponding pass's help page.
"#
)]
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

    /// Run multiple formatting passes at once
    #[clap(long)]
    pass: Vec<PassName>,

    /// Run a single specific formatting pass
    #[clap(subcommand)]
    passes: Option<Passes>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
enum PassName {
    HouseRules,
    Rustfmt,
    UnusedDeps,
    VerifyWorkspace,
    VerifyFuzzers,
    VerifyFlowey,
}

impl PassName {
    fn kebab_case(self) -> &'static str {
        match self {
            PassName::HouseRules => "house-rules",
            PassName::Rustfmt => "rustfmt",
            PassName::UnusedDeps => "unused-deps",
            PassName::VerifyWorkspace => "verify-workspace",
            PassName::VerifyFuzzers => "verify-fuzzers",
            PassName::VerifyFlowey => "verify-flowey",
        }
    }
}

#[derive(clap::Subcommand)]
enum Passes {
    HouseRules(house_rules::HouseRules),
    Rustfmt(rustfmt::Rustfmt),
    UnusedDeps(unused_deps::UnusedDeps),
    VerifyWorkspace(workspace::VerifyWorkspace),
    VerifyFuzzers(crate::tasks::fuzz::VerifyFuzzers),
    VerifyFlowey(verify_flowey::VerifyFlowey),
}

impl Xtask for Fmt {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        // short-circuit if a specific pass was requested
        if let Some(pass) = self.passes {
            if !self.pass.is_empty() {
                anyhow::bail!("cannot use `--pass` when invoking pass directly")
            }

            match pass {
                Passes::UnusedDeps(cmd) => cmd.run(ctx)?,
                Passes::Rustfmt(cmd) => cmd.run(ctx)?,
                Passes::HouseRules(cmd) => cmd.run(ctx)?,
                Passes::VerifyWorkspace(cmd) => cmd.run(ctx)?,
                Passes::VerifyFuzzers(cmd) => cmd.run(ctx)?,
                Passes::VerifyFlowey(cmd) => cmd.run(ctx)?,
            }

            return Ok(());
        }

        // otherwise, run all the formatting passes
        let tasks: Vec<Box<dyn FnOnce() -> anyhow::Result<()> + Send>> = {
            fn wrapper(
                ctx: &crate::XtaskCtx,
                name: &str,
                func: impl FnOnce(crate::XtaskCtx) -> anyhow::Result<()> + Send + 'static,
            ) -> Box<dyn FnOnce() -> anyhow::Result<()> + Send> {
                let ctx = ctx.clone();
                let name = name.to_string();

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

            let fix = self.fix;
            let only_diffed = self.only_diffed;

            let passes = if !self.pass.is_empty() {
                let mut passes = self.pass.clone();
                passes.sort();
                passes.dedup_by(|a, b| a == b);
                passes
            } else {
                // run all of them by default
                vec![
                    PassName::HouseRules,
                    PassName::Rustfmt,
                    PassName::UnusedDeps,
                    PassName::VerifyWorkspace,
                    PassName::VerifyFuzzers,
                    PassName::VerifyFlowey,
                ]
            };

            passes
                .into_iter()
                .map(|pass| {
                    let name = pass.kebab_case();
                    match pass {
                        PassName::HouseRules => wrapper(&ctx, name, {
                            move |ctx| {
                                house_rules::HouseRules::all_passes(fix, only_diffed).run(ctx)
                            }
                        }),
                        PassName::Rustfmt => wrapper(&ctx, name, {
                            move |ctx| rustfmt::Rustfmt::new(fix, only_diffed).run(ctx)
                        }),
                        PassName::UnusedDeps => wrapper(&ctx, name, {
                            move |ctx| unused_deps::UnusedDeps { fix }.run(ctx)
                        }),
                        PassName::VerifyWorkspace => wrapper(&ctx, name, {
                            move |ctx| workspace::VerifyWorkspace.run(ctx)
                        }),
                        PassName::VerifyFuzzers => wrapper(&ctx, name, {
                            move |ctx| crate::tasks::fuzz::VerifyFuzzers.run(ctx)
                        }),
                        PassName::VerifyFlowey => wrapper(&ctx, name, {
                            move |ctx| verify_flowey::VerifyFlowey::new(fix).run(ctx)
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
                        .map(|pass| format!(" --pass {}", pass.kebab_case()))
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
