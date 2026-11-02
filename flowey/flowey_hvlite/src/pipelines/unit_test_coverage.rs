// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Opt-in local unit coverage, independent of PR CI publication.

use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::build_nextest_unit_tests::BuildNextestUnitTestMode;
use flowey_lib_hvlite::collect_unit_test_coverage::Options;
use flowey_lib_hvlite::common::CommonProfile;
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct UnitTestCoverageCli {
    /// Exact workspace package names to test (repeat for multiple packages).
    #[clap(short = 'p', long = "package", required = true)]
    packages: Vec<String>,

    /// Nextest filter expression, within the selected packages.
    #[clap(short = 'E', long)]
    filter: Option<String>,

    /// Fresh result directory. Defaults to a unique directory under target/unit-coverage.
    #[clap(long)]
    output: Option<PathBuf>,

    /// Install missing build dependencies using Flowey's normal setup.
    #[clap(long)]
    install_missing_deps: bool,
}

impl IntoPipeline for UnitTestCoverageCli {
    fn into_pipeline(self, backend: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        anyhow::ensure!(
            matches!(backend, PipelineBackendHint::Local)
                && matches!(FlowPlatform::host(backend), FlowPlatform::Linux(_))
                && matches!(FlowArch::host(backend), FlowArch::X86_64),
            "unit-test-coverage currently requires local x64 Linux"
        );
        for package in &self.packages {
            anyhow::ensure!(
                !package.is_empty()
                    && package
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
                "use an exact workspace package name, not a package pattern: {package}"
            );
        }
        let output = std::path::absolute(self.output.unwrap_or_else(|| {
            crate::repo_root()
                .join("target/unit-coverage")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                ))
        }))?;
        anyhow::ensure!(
            !output.exists(),
            "coverage output already exists; choose a fresh directory: {}",
            output.display()
        );
        let mut pipeline = Pipeline::new();
        pipeline
            .new_job(
                FlowPlatform::host(backend),
                FlowArch::host(backend),
                "local unit test coverage",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init)
            .dep_on(
                |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                    hvlite_repo_source: flowey_lib_common::git_checkout::RepoSource::ExistingClone(
                        ReadVar::from_static(crate::repo_root()),
                    ),
                },
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: self.install_missing_deps,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(false),
                locked: true,
                deny_warnings: false,
                no_incremental: false,
            })
            .dep_on(|ctx| flowey_lib_hvlite::build_nextest_unit_tests::Request {
                target: target_lexicon::triple!("x86_64-unknown-linux-gnu"),
                profile: CommonProfile::Debug,
                build_mode: BuildNextestUnitTestMode::Coverage {
                    packages: self.packages,
                    options: Options {
                        output,
                        filter: self.filter,
                    },
                    done: ctx.new_done_handle(),
                },
            })
            .finish();
        Ok(pipeline)
    }
}
