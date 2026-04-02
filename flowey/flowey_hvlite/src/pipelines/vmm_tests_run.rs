// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pipeline to discover artifacts and run VMM tests in a single command.
//!
//! This combines the functionality of `vmm-tests-discover` and `vmm-tests` into
//! a single convenient pipeline that:
//! 1. Discovers required artifacts for the specified test filter (at pipeline
//!    construction time)
//! 2. Builds the necessary dependencies
//! 3. Runs the tests

use crate::pipelines::vmm_tests::VmmTestTargetCli;
use crate::pipelines::vmm_tests::VmmTestsPipelineOptions;
use crate::pipelines::vmm_tests::build_vmm_tests_pipeline;
use crate::pipelines::vmm_tests::resolve_target;
use crate::pipelines::vmm_tests::selections_from_resolved;
use crate::pipelines::vmm_tests::validate_wsl_dir;
use anyhow::Context as _;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::_jobs::local_discover_vmm_tests_artifacts::discover_artifacts_sync;
use flowey_lib_hvlite::artifact_to_build_mapping::ResolvedArtifactSelections;
use std::path::PathBuf;

/// Build and run VMM tests with automatic artifact discovery
///
/// This is a convenience command that combines `vmm-tests-discover` and `vmm-tests`
/// into a single step. It automatically discovers required artifacts for the
/// specified filter and then builds and runs the tests.
///
/// Example usage:
///   cargo xflowey vmm-tests-run --filter "test(ubuntu)" --target windows-x64 --dir /mnt/q/vmm_tests_out/
#[derive(clap::Args)]
pub struct VmmTestsRunCli {
    /// Specify what target to build the VMM tests for
    ///
    /// If not specified, defaults to the current host target.
    #[clap(long)]
    target: Option<VmmTestTargetCli>,

    /// Directory for the output artifacts
    #[clap(long)]
    dir: PathBuf,

    /// Test filter (nextest filter expression)
    ///
    /// Examples:
    ///   - `test(alpine)` - run tests with "alpine" in the name
    ///   - `test(/^boot_/)` - run tests starting with "boot_"
    ///   - `all()` - run all tests
    #[clap(long, default_value = "all()")]
    filter: String,

    /// pass `--verbose` to cargo
    #[clap(long)]
    verbose: bool,
    /// Automatically install any missing required dependencies.
    #[clap(long)]
    install_missing_deps: bool,

    /// Use unstable WHP interfaces
    #[clap(long)]
    unstable_whp: bool,
    /// Release build instead of debug build
    #[clap(long)]
    release: bool,

    /// Build only, do not run
    #[clap(long)]
    build_only: bool,
    /// Copy extras to output dir (symbols, etc)
    #[clap(long)]
    copy_extras: bool,

    /// Skip the interactive VHD download prompt
    #[clap(long)]
    skip_vhd_prompt: bool,

    /// Optional: custom kernel modules
    #[clap(long)]
    custom_kernel_modules: Option<PathBuf>,
    /// Optional: custom kernel image
    #[clap(long)]
    custom_kernel: Option<PathBuf>,
}

impl IntoPipeline for VmmTestsRunCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("vmm-tests-run is for local use only")
        }

        let Self {
            target,
            dir,
            filter,
            verbose,
            install_missing_deps,
            unstable_whp,
            release,
            build_only,
            copy_extras,
            custom_kernel_modules,
            custom_kernel,
            skip_vhd_prompt,
        } = self;

        // 1. Resolve target
        let target = resolve_target(target, backend_hint)?;
        let target_os = target.as_triple().operating_system;
        let target_architecture = target.as_triple().architecture;
        let target_str = target.as_triple().to_string();

        // 2. Validate output directory for WSL
        validate_wsl_dir(&dir, target_os)?;
        std::fs::create_dir_all(&dir).context("failed to create output directory")?;

        // 3. Run artifact discovery inline at pipeline construction time
        log::info!("Step 1: Discovering required artifacts...");
        let repo_root = crate::repo_root();
        let artifacts_json = discover_artifacts_sync(&repo_root, &target_str, &filter, release)
            .context("during artifact discovery")?;

        // 4. Resolve to build selections
        let resolved = ResolvedArtifactSelections::from_artifact_list_json(
            &artifacts_json,
            target_architecture,
            target_os,
        )
        .context("failed to parse discovered artifacts")?;

        if !resolved.unknown.is_empty() {
            anyhow::bail!(
                "Unknown artifacts found (mapping needs to be updated):\n  {}",
                resolved.unknown.join("\n  ")
            );
        }

        log::info!("Resolved build selections: {:?}", resolved.build);
        log::info!(
            "Resolved downloads: {:?}",
            resolved.downloads.iter().collect::<Vec<_>>()
        );

        let selections = selections_from_resolved(filter, resolved, target_os);

        // 5. Construct and return the pipeline
        log::info!("Step 2: Building and running tests...");
        build_vmm_tests_pipeline(
            backend_hint,
            target,
            selections,
            dir,
            VmmTestsPipelineOptions {
                verbose,
                install_missing_deps,
                unstable_whp,
                release,
                build_only,
                copy_extras,
                skip_vhd_prompt,
                custom_kernel_modules,
                custom_kernel,
            },
        )
    }
}
