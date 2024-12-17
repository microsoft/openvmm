// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A set of CLI commands which are common to _all_ flowey implementations.

use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use flowey_core::node::FlowBackend;
use flowey_core::pipeline::IntoPipeline;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;

pub mod debug;
pub mod exec_snippet;
pub mod pipeline;
pub mod regen;
pub mod var_db;

#[derive(Parser)]
#[clap(name = "flowey", about = "backend-agnostic declarative flow runner")]
struct Cli<P: Subcommand> {
    #[clap(subcommand)]
    command: Commands<P>,
}

#[derive(Subcommand)]
enum Commands<P: Subcommand> {
    Pipeline(pipeline::Pipeline<P>),
    Regen(regen::Regen),

    #[clap(hide = true, alias = "e")]
    ExecSnippet(exec_snippet::ExecSnippet),
    #[clap(hide = true, alias = "v")]
    VarDb(var_db::VarDb),

    #[clap(subcommand)]
    Debug(debug::DebugCommands),
}

pub fn cli_main<P: Subcommand + IntoPipeline>(
    flowey_crate: &str,
    repo_root: &Path,
) -> anyhow::Result<()> {
    let cli = Cli::<P>::parse();

    // Check if the runtime variable DB includes a "FLOWEY_LOG" variable, and
    // use that to override the log level.
    //
    // This mechanism is in-place because some YAML pipeline defn langs (*cough*
    // ADO *cough*) don't let you set pipeline-level env vars which are
    // automatically inherited by all shell contexts.
    let mut log_override = None;
    if let Commands::VarDb(var_db::VarDb { job_idx, .. })
    | Commands::ExecSnippet(exec_snippet::ExecSnippet { job_idx, .. }) = &cli.command
    {
        log_override = try_get_flowey_log(*job_idx).unwrap_or_default();
    }

    if let Some(log_level) = log_override {
        ci_logger::init_value(&log_level).unwrap();
    } else {
        ci_logger::init("FLOWEY_LOG").unwrap();
    }

    match cli.command {
        Commands::Debug(cmd) => cmd.run(),
        Commands::Pipeline(cmd) => cmd.run(flowey_crate, repo_root),
        Commands::Regen(cmd) => cmd.run(repo_root),
        Commands::ExecSnippet(cmd) => cmd.run(),
        Commands::VarDb(cmd) => cmd.run(),
    }
}

#[derive(Copy, Clone, ValueEnum, Serialize, Deserialize)]
pub enum FlowBackendCli {
    Local,
    Ado,
    Github,
}

impl From<FlowBackendCli> for FlowBackend {
    fn from(v: FlowBackendCli) -> FlowBackend {
        match v {
            FlowBackendCli::Local => FlowBackend::Local,
            FlowBackendCli::Ado => FlowBackend::Ado,
            FlowBackendCli::Github => FlowBackend::Github,
        }
    }
}

fn try_get_flowey_log(job_idx: usize) -> anyhow::Result<Option<String>> {
    // skip if the env var is already set
    if std::env::var("FLOWEY_LOG").is_err() {
        let log_level = var_db::open_var_db(job_idx)?
            .try_get_var("FLOWEY_LOG")
            .map(|val| {
                serde_json::from_slice::<String>(&val)
                    .expect("found FLOWEY_LOG in db, but it wasn't a json string!")
            });

        if let Some(log_level) = log_level {
            return Ok(Some(log_level));
        }
    }

    Ok(None)
}
