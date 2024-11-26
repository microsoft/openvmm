// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use flowey_core::node::FlowBackend;
use flowey_core::pipeline::IntoPipeline;
use flowey_core::pipeline::PipelineBackendHint;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, clap::ValueEnum)]
pub enum VizModeCli {
    Toposort,
    Dot,
    FlowDot,
}

pub(crate) enum CheckMode {
    Runtime(PathBuf),
    Check(PathBuf),
    None,
}

#[derive(clap::Subcommand)]
enum PipelineBackendCli<P: clap::Subcommand> {
    /// A locally executable bash script
    #[clap(subcommand_value_name = "PIPELINE")]
    #[clap(subcommand_help_heading = "Pipeline")]
    Bash {
        /// Output directory to write pipeline scripts to. If the directory
        /// doesn't exist, it will be created.
        #[clap(long, default_value = "./flowey-out")]
        out_dir: PathBuf,

        /// Persistent storage directory shared across multiple runs. If the
        /// directory doesn't exist, it will be created.
        #[clap(long, default_value = "./flowey-persist")]
        persist_dir: PathBuf,

        /// Enable flowey internal debug logs at runtime
        #[clap(help_heading = "Global Options (flowey)", global = true, long)]
        runtime_debug_log: bool,

        /// Attempt to run windows jobs on WSL2. This may or may not work,
        /// depending on if the flowey nodes at play are resilient to running
        /// in WSL2.
        #[clap(help_heading = "Global Options (flowey)", global = true, long)]
        windows_as_wsl: bool,

        #[clap(subcommand)]
        pipelines: P,
    },
    /// An ADO pipeline YAML file
    #[clap(subcommand_value_name = "PIPELINE")]
    #[clap(subcommand_help_heading = "Pipeline")]
    Ado {
        #[clap(subcommand)]
        pipelines: P,

        /// disable flowey internal debug logs at runtime
        #[clap(help_heading = "Global Options (flowey)", global = true, long)]
        no_runtime_debug_log: bool,

        /// repo-root relative path to generated YAML file
        #[clap(long)]
        out: PathBuf,

        /// check that the provided YAML matches the generated YAML.
        #[clap(long, value_name = "YAML")]
        check: Option<PathBuf>,

        /// generate the pipeline JSON, also runs check
        #[clap(long, value_name = "YAML")]
        runtime: Option<PathBuf>,
    },
    /// A GitHub pipeline YAML file
    #[clap(subcommand_value_name = "PIPELINE")]
    #[clap(subcommand_help_heading = "Pipeline")]
    Github {
        #[clap(subcommand)]
        pipelines: P,

        /// disable flowey internal debug logs at runtime
        #[clap(help_heading = "Global Options (flowey)", global = true, long)]
        no_runtime_debug_log: bool,

        /// repo-root relative path to generated YAML file
        #[clap(long)]
        out: PathBuf,

        /// check that the provided YAML matches the generated YAML.
        #[clap(long, value_name = "YAML")]
        check: Option<PathBuf>,

        /// generate the pipeline JSON, also runs check
        #[clap(long, value_name = "YAML", conflicts_with = "check")]
        runtime: Option<PathBuf>,
    },
    /// Run the pipeline directly using flowey
    Run {
        #[clap(subcommand)]
        pipelines: P,

        /// Output directory to emit artifacts into. If the directory
        /// doesn't exist, it will be created.
        #[clap(long, default_value = "./flowey-out")]
        out_dir: PathBuf,

        /// Persistent storage directory shared across multiple runs. If the
        /// directory doesn't exist, it will be created.
        #[clap(long, default_value = "./flowey-persist")]
        persist_dir: PathBuf,

        /// Attempt to run windows jobs on WSL2. This may or may not work,
        /// depending on if the flowey nodes at play are resilient to running
        /// in WSL2.
        #[clap(help_heading = "Global Options (flowey)", global = true, long)]
        windows_as_wsl: bool,
    },
}

/// Generate and Run pipelines.
#[derive(clap::Args)]
#[clap(subcommand_help_heading = "Pipeline Kind")]
#[clap(subcommand_value_name = "PIPELINE_KIND")]
pub struct Pipeline<P: clap::Subcommand> {
    /// (debug) Emit a visualization of the output flow, instead of the flow
    /// itself.
    #[clap(help_heading = "Global Options (flowey)", global = true, long)]
    viz_mode: Option<VizModeCli>,

    /// (debug) Filter the pipeline to only include the specified jobs.
    ///
    /// At this time, this will _not_ allow running a job without also running
    /// any jobs it depends on!
    ///
    /// Accepts a comma-separated list of "job ids", a list of which can be
    /// obtained by running `--include-jobs='?'`
    ///
    /// NOTE: because this is intended as a debugging tool, there is no
    /// mechanism to ensure that "job ids" remain stable in the face of pipeline
    /// updates / flowey updates. i.e: an `--include-jobs` invocation that works
    /// today may not work after making changes to the pipeline definition /
    /// updating flowey.
    #[clap(help_heading = "Global Options (flowey)", global = true, long)]
    #[expect(clippy::option_option, reason = "For clap derive")]
    include_jobs: Option<Option<IncludeJobs>>,

    #[clap(subcommand)]
    project_pipeline: PipelineBackendCli<P>,
}

#[derive(Clone)]
enum IncludeJobs {
    Query,
    List(Vec<usize>),
}

impl std::str::FromStr for IncludeJobs {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "?" {
            return Ok(IncludeJobs::Query);
        }

        let mut list = Vec::new();
        for n in s.split(',') {
            if n == "?" {
                return Err("can only pass '?' once");
            }

            list.push(
                n.parse()
                    .map_err(|_| "expected comma separated list of numbers")?,
            );
        }
        Ok(IncludeJobs::List(list))
    }
}

impl<P: clap::Subcommand + IntoPipeline> Pipeline<P> {
    pub fn run(self, flowey_crate: &str, repo_root: &Path) -> anyhow::Result<()> {
        let Self {
            project_pipeline,
            viz_mode,
            include_jobs,
        } = self;

        match project_pipeline {
            PipelineBackendCli::Bash {
                pipelines,
                out_dir,
                persist_dir,
                runtime_debug_log,
                windows_as_wsl,
            } => {
                let mut resolved_pipeline =
                    resolve_pipeline(pipelines, PipelineBackendHint::Local)?;

                if matches!(
                    resolve_include_jobs(&mut resolved_pipeline, include_jobs)?,
                    EarlyExit::Yes
                ) {
                    return Ok(());
                }

                if let Some(viz_mode) = viz_mode {
                    viz_pipeline(
                        viz_mode,
                        resolved_pipeline,
                        FlowBackend::Local,
                        crate::running_in_wsl(),
                    )
                } else {
                    let _ = (out_dir, persist_dir, runtime_debug_log, windows_as_wsl);
                    todo!("bash backend is not actively maintained, and currently broken")
                }
            }
            PipelineBackendCli::Run {
                pipelines,
                out_dir,
                persist_dir,
                windows_as_wsl,
            } => {
                let mut resolved_pipeline =
                    resolve_pipeline(pipelines, PipelineBackendHint::Local)?;

                if matches!(
                    resolve_include_jobs(&mut resolved_pipeline, include_jobs)?,
                    EarlyExit::Yes
                ) {
                    return Ok(());
                }

                if let Some(viz_mode) = viz_mode {
                    viz_pipeline(
                        viz_mode,
                        resolved_pipeline,
                        FlowBackend::Local,
                        crate::running_in_wsl(),
                    )
                } else {
                    crate::pipeline_resolver::direct_run::direct_run(
                        resolved_pipeline,
                        windows_as_wsl,
                        out_dir,
                        persist_dir,
                    )
                }
            }
            PipelineBackendCli::Ado {
                pipelines,
                out,
                no_runtime_debug_log,
                check,
                runtime,
            } => {
                let mut resolved_pipeline = resolve_pipeline(pipelines, PipelineBackendHint::Ado)?;

                if matches!(
                    resolve_include_jobs(&mut resolved_pipeline, include_jobs)?,
                    EarlyExit::Yes
                ) {
                    return Ok(());
                }

                if let Some(viz_mode) = viz_mode {
                    viz_pipeline(viz_mode, resolved_pipeline, FlowBackend::Ado, false)
                } else {
                    let mode = if let Some(runtime_path) = runtime {
                        CheckMode::Runtime(runtime_path)
                    } else if let Some(check_path) = check {
                        CheckMode::Check(check_path)
                    } else {
                        CheckMode::None
                    };

                    crate::pipeline_resolver::ado_yaml::ado_yaml(
                        resolved_pipeline,
                        !no_runtime_debug_log,
                        repo_root,
                        &out,
                        flowey_crate,
                        mode,
                    )
                }
            }
            PipelineBackendCli::Github {
                pipelines,
                out,
                no_runtime_debug_log,
                check,
                runtime,
            } => {
                let mut resolved_pipeline =
                    resolve_pipeline(pipelines, PipelineBackendHint::Github)?;

                if matches!(
                    resolve_include_jobs(&mut resolved_pipeline, include_jobs)?,
                    EarlyExit::Yes
                ) {
                    return Ok(());
                }

                if let Some(viz_mode) = viz_mode {
                    viz_pipeline(viz_mode, resolved_pipeline, FlowBackend::Github, false)
                } else {
                    let mode = if let Some(runtime_path) = runtime {
                        CheckMode::Runtime(runtime_path)
                    } else if let Some(check_path) = check {
                        CheckMode::Check(check_path)
                    } else {
                        CheckMode::None
                    };

                    crate::pipeline_resolver::github_yaml::github_yaml(
                        resolved_pipeline,
                        !no_runtime_debug_log,
                        repo_root,
                        &out,
                        flowey_crate,
                        mode,
                    )
                }
            }
        }
    }
}

fn resolve_pipeline<P: IntoPipeline>(
    pipelines: P,
    backend_hint: PipelineBackendHint,
) -> Result<crate::pipeline_resolver::generic::ResolvedPipeline, anyhow::Error> {
    let pipeline = pipelines
        .into_pipeline(backend_hint)
        .context("error defining pipeline")?;

    let resolved_pipeline = crate::pipeline_resolver::generic::resolve_pipeline(pipeline)
        .context("invalid pipeline")?;

    Ok(resolved_pipeline)
}

fn viz_pipeline(
    viz_mode: VizModeCli,
    resolved_pipeline: crate::pipeline_resolver::generic::ResolvedPipeline,
    backend: FlowBackend,
    with_persist_dir: bool,
) -> Result<(), anyhow::Error> {
    match viz_mode {
        VizModeCli::Toposort => crate::pipeline_resolver::viz::viz_pipeline_toposort(
            resolved_pipeline,
            backend,
            with_persist_dir,
        ),
        VizModeCli::Dot => {
            crate::pipeline_resolver::viz::viz_pipeline_dot(resolved_pipeline, backend)
        }
        VizModeCli::FlowDot => crate::pipeline_resolver::viz::viz_pipeline_flow_dot(
            resolved_pipeline,
            backend,
            with_persist_dir,
        ),
    }
}

enum EarlyExit {
    Yes,
    No,
}

#[expect(clippy::option_option, reason = "For clap derive")]
fn resolve_include_jobs(
    resolved_pipeline: &mut crate::pipeline_resolver::generic::ResolvedPipeline,
    include_jobs: Option<Option<IncludeJobs>>,
) -> anyhow::Result<EarlyExit> {
    let Some(include_jobs) = include_jobs else {
        return Ok(EarlyExit::No);
    };

    match include_jobs.unwrap_or(IncludeJobs::Query) {
        IncludeJobs::Query => {
            for (present_idx, &graph_idx) in resolved_pipeline.order.iter().enumerate() {
                println!(
                    "{}: {}",
                    present_idx, resolved_pipeline.graph[graph_idx].label
                );
            }
            Ok(EarlyExit::Yes)
        }
        IncludeJobs::List(list) => {
            let preserve_jobs = list
                .into_iter()
                .map(|present_idx| resolved_pipeline.order.get(present_idx).cloned())
                .collect::<Option<Vec<_>>>()
                .context("passed invalid job idx. use '?' to list available jobs")?;
            resolved_pipeline.trim_pipeline_graph(preserve_jobs);
            Ok(EarlyExit::No)
        }
    }
}
