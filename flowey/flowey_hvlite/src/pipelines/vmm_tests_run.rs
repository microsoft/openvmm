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
use crate::pipelines::vmm_tests::validate_output_dir;
use anyhow::Context as _;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::artifact_to_build_mapping::ResolvedArtifactSelections;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

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
        validate_output_dir(&dir, target_os)?;
        std::fs::create_dir_all(&dir).context("failed to create output directory")?;

        // 3. Run artifact discovery inline at pipeline construction time
        log::info!("Step 1: Discovering required artifacts...");
        let repo_root = crate::repo_root();
        let artifacts_json = discover_artifacts(&repo_root, &target_str, &filter, release)
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

/// Run artifact discovery by invoking `cargo nextest list` and the test
/// binary's `--list-required-artifacts` flag.
///
/// Returns the raw JSON string describing required/optional artifacts.
fn discover_artifacts(
    repo_root: &Path,
    target: &str,
    filter: &str,
    release: bool,
) -> anyhow::Result<String> {
    // Check that cargo-nextest is available
    let nextest_check = Command::new("cargo")
        .args(["nextest", "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match nextest_check {
        Ok(status) if status.success() => {}
        _ => anyhow::bail!("cargo-nextest not found. Run 'cargo xflowey restore-packages' first."),
    }

    log::info!(
        "Discovering artifacts for filter: {} (target: {})",
        filter,
        target
    );

    // Step 1: Use nextest to resolve the filter expression to test names and
    // get the binary path
    let mut cmd = Command::new("cargo");
    cmd.current_dir(repo_root).args([
        "nextest",
        "list",
        "-p",
        "vmm_tests",
        "--target",
        target,
        "--filter-expr",
        filter,
        "--message-format",
        "json",
    ]);
    if release {
        cmd.arg("--release");
    }
    let nextest_output = cmd.output().context("failed to run cargo nextest list")?;
    anyhow::ensure!(
        nextest_output.status.success(),
        "cargo nextest list failed: {}",
        String::from_utf8_lossy(&nextest_output.stderr)
    );
    let nextest_stdout = String::from_utf8(nextest_output.stdout)
        .map_err(|e| anyhow::anyhow!("nextest output is not valid UTF-8: {}", e))?;
    let (test_binary, test_names) = parse_nextest_output(&nextest_stdout)?;

    if test_names.is_empty() {
        log::warn!("No tests match the filter: {}", filter);
        let empty_output = serde_json::json!({
            "target": target,
            "required": [],
            "optional": []
        });
        return Ok(serde_json::to_string_pretty(&empty_output)?);
    }

    log::info!("Found {} matching tests", test_names.len());
    for name in &test_names {
        log::debug!("  - {}", name);
    }

    // Step 2: Query petri for artifacts of each matching test
    log::info!("Using test binary: {}", test_binary.display());
    log::info!(
        "Querying artifacts for {} tests in a single invocation",
        test_names.len()
    );
    let stdin_data = test_names
        .iter()
        .map(|n| format!("{n}\n"))
        .collect::<String>();
    let mut child = Command::new(&test_binary)
        .args(["--list-required-artifacts", "--tests-from-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn test binary")?;

    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(stdin_data.as_bytes())
        .context("failed to write test names to stdin")?;

    let artifact_output = child
        .wait_with_output()
        .context("failed to wait for test binary")?;
    anyhow::ensure!(
        artifact_output.status.success(),
        "test binary failed: {}",
        String::from_utf8_lossy(&artifact_output.stderr)
    );
    let artifact_stdout = String::from_utf8(artifact_output.stdout)
        .map_err(|e| anyhow::anyhow!("test output is not valid UTF-8: {}", e))?;

    parse_artifacts_output(&artifact_stdout, target)
}

/// Parse `cargo nextest list --message-format json` output to extract test
/// names and binary path.
fn parse_nextest_output(stdout: &str) -> anyhow::Result<(PathBuf, Vec<String>)> {
    let json: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| anyhow::anyhow!("failed to parse nextest JSON output: {}", e))?;

    let mut test_names = Vec::new();
    let mut binary_path = None;

    // Navigate to rust-suites -> vmm_tests::tests -> testcases
    if let Some(vmm_tests) = json
        .get("rust-suites")
        .and_then(|s| s.get("vmm_tests::tests"))
    {
        if let Some(path) = vmm_tests.get("binary-path").and_then(|v| v.as_str()) {
            binary_path = Some(PathBuf::from(path));
        }

        if let Some(testcases_obj) = vmm_tests.get("testcases").and_then(|t| t.as_object()) {
            for (test_name, test_info) in testcases_obj {
                let matches = test_info
                    .get("filter-match")
                    .and_then(|fm| fm.get("status"))
                    .and_then(|s| s.as_str())
                    == Some("matches");

                if matches {
                    test_names.push(test_name.clone());
                }
            }
        }
    }

    let binary_path = binary_path
        .ok_or_else(|| anyhow::anyhow!("Could not find test binary path in nextest output"))?;

    Ok((binary_path, test_names))
}

/// Parse test binary `--list-required-artifacts` JSON output and add target
/// info.
fn parse_artifacts_output(stdout: &str, target: &str) -> anyhow::Result<String> {
    let json: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| anyhow::anyhow!("failed to parse test output JSON: {}", e))?;

    let required = json
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let optional = json
        .get("optional")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let output = serde_json::json!({
        "target": target,
        "required": required,
        "optional": optional,
    });

    Ok(serde_json::to_string_pretty(&output)?)
}
