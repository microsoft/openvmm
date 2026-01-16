// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Combined pipeline to discover artifacts and run VMM tests in a single command.
//!
//! This combines the functionality of `vmm-tests-discover` and `vmm-tests` into
//! a single convenient command that:
//! 1. Discovers required artifacts for the specified test filter
//! 2. Builds the necessary dependencies
//! 3. Runs the tests

use crate::pipelines::vmm_tests::VmmTestTargetCli;
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
    ///   - `test(ubuntu)` - run tests with "ubuntu" in the name
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

    /// Optional: custom kernel modules
    #[clap(long)]
    custom_kernel_modules: Option<PathBuf>,
    /// Optional: custom kernel image
    #[clap(long)]
    custom_kernel: Option<PathBuf>,
}

impl VmmTestsRunCli {
    /// Execute the combined discover + run workflow
    pub fn run(self) -> anyhow::Result<()> {
        use anyhow::Context;

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
        } = self;

        // Create output directory if it doesn't exist
        std::fs::create_dir_all(&dir).context("failed to create output directory")?;

        // Use a deterministic path in the output directory for the artifacts file
        let artifacts_file = dir.join(".vmm_tests_artifacts.json");

        // Build the target argument
        let target_arg = target.map(|t| match t {
            VmmTestTargetCli::WindowsAarch64 => "windows-aarch64",
            VmmTestTargetCli::WindowsX64 => "windows-x64",
            VmmTestTargetCli::LinuxX64 => "linux-x64",
        });

        // Step 1: Run vmm-tests-discover
        log::info!("Step 1: Discovering required artifacts...");
        let mut discover_cmd = std::process::Command::new("cargo");
        discover_cmd
            .arg("xflowey")
            .arg("vmm-tests-discover")
            .arg("--filter")
            .arg(&filter)
            .arg("--output")
            .arg(&artifacts_file);

        if let Some(target) = target_arg {
            discover_cmd.arg("--target").arg(target);
        }
        if release {
            discover_cmd.arg("--release");
        }
        if verbose {
            discover_cmd.arg("--verbose");
        }

        discover_cmd.current_dir(crate::repo_root());

        log::info!("Running: {:?}", discover_cmd);
        let status = discover_cmd
            .status()
            .context("failed to run vmm-tests-discover")?;

        if !status.success() {
            anyhow::bail!(
                "vmm-tests-discover failed with exit code: {:?}",
                status.code()
            );
        }

        log::info!("Artifacts file written to: {}", artifacts_file.display());

        // Step 2: Run vmm-tests with the discovered artifacts
        log::info!("Step 2: Building and running tests...");
        let mut test_cmd = std::process::Command::new("cargo");
        test_cmd
            .arg("xflowey")
            .arg("vmm-tests")
            .arg("--filter")
            .arg(&filter)
            .arg("--artifacts-file")
            .arg(&artifacts_file)
            .arg("--dir")
            .arg(&dir);

        if let Some(target) = target_arg {
            test_cmd.arg("--target").arg(target);
        }
        if verbose {
            test_cmd.arg("--verbose");
        }
        if install_missing_deps {
            test_cmd.arg("--install-missing-deps");
        }
        if unstable_whp {
            test_cmd.arg("--unstable-whp");
        }
        if release {
            test_cmd.arg("--release");
        }
        if build_only {
            test_cmd.arg("--build-only");
        }
        if copy_extras {
            test_cmd.arg("--copy-extras");
        }
        if let Some(kernel_modules) = custom_kernel_modules {
            test_cmd.arg("--custom-kernel-modules").arg(kernel_modules);
        }
        if let Some(kernel) = custom_kernel {
            test_cmd.arg("--custom-kernel").arg(kernel);
        }

        test_cmd.current_dir(crate::repo_root());

        log::info!("Running: {:?}", test_cmd);
        let status = test_cmd.status().context("failed to run vmm-tests")?;

        if !status.success() {
            anyhow::bail!("vmm-tests failed with exit code: {:?}", status.code());
        }

        log::info!("VMM tests completed successfully!");
        Ok(())
    }
}
