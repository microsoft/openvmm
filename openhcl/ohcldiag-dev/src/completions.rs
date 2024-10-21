// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::new_client;
use crate::VmArg;
use crate::VmId;
use clap::Parser;
use clap_dyn_complete::CustomCompleter;
use clap_dyn_complete::CustomCompleterFactory;
use pal_async::DefaultDriver;
use std::time::Duration;

/// Enable shell-completions
#[derive(Parser)]
pub struct Completions {
    /// Shell to generate completions for
    shell: clap_dyn_complete::Shell,
}

impl Completions {
    pub fn run(self) -> anyhow::Result<()> {
        clap_dyn_complete::emit_completion_stub(
            self.shell,
            "ohcldiag-dev",
            ". complete",
            &mut std::io::stdout(),
        )?;
        Ok(())
    }
}

pub(crate) struct OhcldiagDevCompleteFactory {
    pub driver: DefaultDriver,
}

impl CustomCompleterFactory for OhcldiagDevCompleteFactory {
    type CustomCompleter = OhcldiagDevComplete;
    async fn build(&self, ctx: &clap_dyn_complete::RootCtx<'_>) -> Self::CustomCompleter {
        let vm = ctx.matches.try_get_one::<VmId>("VM").unwrap_or_default();
        let client = if let Some(vm) = vm {
            new_client(self.driver.clone(), &VmArg { id: vm.clone() })
                .await
                .ok()
        } else {
            None
        };

        OhcldiagDevComplete { client }
    }
}

pub(crate) struct OhcldiagDevComplete {
    client: Option<diag_client::DiagClient>,
}

impl CustomCompleter for OhcldiagDevComplete {
    async fn complete(
        &self,
        ctx: &clap_dyn_complete::RootCtx<'_>,
        subcommand_path: &[&str],
        arg_id: &str,
    ) -> Vec<String> {
        match (subcommand_path, arg_id) {
            (["ohcldiag-dev"], "VM") => list_vms().unwrap_or_default(),
            (["ohcldiag-dev", "inspect"], "path") => {
                let on_error = vec!["failed/to/connect".into()];

                let (parent_path, to_complete) = (ctx.to_complete)
                    .rsplit_once('/')
                    .unwrap_or(("", ctx.to_complete));

                let Some(client) = self.client.as_ref() else {
                    return on_error;
                };

                let Ok(node) = client
                    .inspect(parent_path, Some(1), Some(Duration::from_secs(1)))
                    .await
                else {
                    return on_error;
                };

                let mut completions = Vec::new();

                if let inspect::Node::Dir(dir) = node {
                    for entry in dir {
                        if entry.name.starts_with(to_complete) {
                            if parent_path.is_empty() {
                                completions.push(format!("{}/", entry.name))
                            } else {
                                completions.push(format!(
                                    "{}/{}{}",
                                    parent_path,
                                    entry.name,
                                    if matches!(entry.node, inspect::Node::Dir(..)) {
                                        "/"
                                    } else {
                                        ""
                                    }
                                ))
                            }
                        }
                    }
                }
                completions
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(windows)]
fn list_vms() -> anyhow::Result<Vec<String>> {
    use anyhow::Context;
    let output = std::process::Command::new("hvc.exe")
        .arg("list")
        .arg("-q")
        .output()
        .context("failed to invoke hvc.exe")?;

    if output.status.success() {
        let stdout = std::str::from_utf8(&output.stdout).context("stdout isn't utf8")?;
        Ok(stdout.trim().lines().map(String::from).collect())
    } else {
        Ok(vec![".".into()])
    }
}

#[cfg(not(windows))]
fn list_vms() -> anyhow::Result<Vec<String>> {
    Ok(vec![".".into()])
}
