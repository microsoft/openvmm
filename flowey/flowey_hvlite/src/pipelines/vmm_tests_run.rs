// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pipeline to discover artifacts and run VMM tests in a single command.
//!
//! This pipeline:
//! 1. Discovers required artifacts for the specified test filter (at pipeline
//!    construction time)
//! 2. Builds the necessary dependencies
//! 3. Runs the tests

use anyhow::Context as _;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::VmmTestSelections;
use flowey_lib_hvlite::artifact_to_build_mapping::ResolvedArtifactSelections;
use flowey_lib_hvlite::install_vmm_tests_deps::VmmTestsDepSelections;
use flowey_lib_hvlite::run_cargo_build::common::CommonArch;
use flowey_lib_hvlite::run_cargo_build::common::CommonTriple;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

/// Build and run VMM tests with automatic artifact discovery
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
    /// Optional: custom UEFI firmware (MSVM.fd) to use instead of the
    /// downloaded release. Path to a locally-built MSVM.fd file.
    #[clap(long)]
    custom_uefi_firmware: Option<PathBuf>,
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
            skip_vhd_prompt,
            custom_kernel_modules,
            custom_kernel,
            custom_uefi_firmware,
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
                custom_uefi_firmware,
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
    log::info!("Querying artifacts for {} tests", test_names.len());
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

// Target resolution and pipeline construction helpers

#[derive(clap::ValueEnum, Copy, Clone)]
enum VmmTestTargetCli {
    /// Windows Aarch64
    WindowsAarch64,
    /// Windows X64
    WindowsX64,
    /// Linux X64
    LinuxX64,
}

/// Resolve a CLI target option to a CommonTriple, defaulting to the host.
fn resolve_target(
    target: Option<VmmTestTargetCli>,
    backend_hint: PipelineBackendHint,
) -> anyhow::Result<CommonTriple> {
    let target = if let Some(t) = target {
        t
    } else {
        match (
            FlowArch::host(backend_hint),
            FlowPlatform::host(backend_hint),
        ) {
            (FlowArch::Aarch64, FlowPlatform::Windows) => VmmTestTargetCli::WindowsAarch64,
            (FlowArch::X86_64, FlowPlatform::Windows) => VmmTestTargetCli::WindowsX64,
            (FlowArch::X86_64, FlowPlatform::Linux(_)) => VmmTestTargetCli::LinuxX64,
            _ => anyhow::bail!("unsupported host"),
        }
    };

    Ok(match target {
        VmmTestTargetCli::WindowsAarch64 => CommonTriple::AARCH64_WINDOWS_MSVC,
        VmmTestTargetCli::WindowsX64 => CommonTriple::X86_64_WINDOWS_MSVC,
        VmmTestTargetCli::LinuxX64 => CommonTriple::X86_64_LINUX_GNU,
    })
}

/// Validate the output directory path based on the current platform.
///
/// When running under WSL and targeting Windows, the output directory must be a
/// Windows-accessible path (DrvFs mount like `/mnt/c/...`) because Windows
/// requires VHDs to reside on a Windows filesystem. On native Windows or Linux
/// this check is a no-op.
fn validate_output_dir(
    dir: &Path,
    target_os: target_lexicon::OperatingSystem,
) -> anyhow::Result<()> {
    if flowey_cli::running_in_wsl()
        && matches!(target_os, target_lexicon::OperatingSystem::Windows)
        && !flowey_cli::is_wsl_windows_path(dir)
    {
        anyhow::bail!(
            "When targeting Windows from WSL, --dir must be a path on Windows \
                 (i.e., on a DrvFs mount like /mnt/c/vmm_tests). \
                 Got: {}",
            dir.display()
        );
    }
    Ok(())
}

/// Resolve `ResolvedArtifactSelections` to `VmmTestSelections`.
fn selections_from_resolved(
    filter: String,
    resolved: ResolvedArtifactSelections,
    target_os: target_lexicon::OperatingSystem,
) -> VmmTestSelections {
    VmmTestSelections {
        filter,
        artifacts: resolved.downloads.into_iter().collect(),
        build: resolved.build.clone(),
        deps: match target_os {
            target_lexicon::OperatingSystem::Windows => VmmTestsDepSelections::Windows {
                hyperv: true,
                whp: resolved.build.openvmm,
                hardware_isolation: resolved.build.prep_steps,
            },
            target_lexicon::OperatingSystem::Linux => VmmTestsDepSelections::Linux,
            _ => unreachable!(),
        },
        needs_release_igvm: resolved.needs_release_igvm,
    }
}

struct VmmTestsPipelineOptions {
    verbose: bool,
    install_missing_deps: bool,
    unstable_whp: bool,
    release: bool,
    build_only: bool,
    copy_extras: bool,
    skip_vhd_prompt: bool,
    custom_kernel_modules: Option<PathBuf>,
    custom_kernel: Option<PathBuf>,
    custom_uefi_firmware: Option<PathBuf>,
}

/// Construct the pipeline job for building and running VMM tests.
fn build_vmm_tests_pipeline(
    backend_hint: PipelineBackendHint,
    target: CommonTriple,
    selections: VmmTestSelections,
    dir: PathBuf,
    opts: VmmTestsPipelineOptions,
) -> anyhow::Result<Pipeline> {
    let target_architecture = target.as_triple().architecture;
    let recipe_arch = match target_architecture {
        target_lexicon::Architecture::X86_64 => CommonArch::X86_64,
        target_lexicon::Architecture::Aarch64(_) => CommonArch::Aarch64,
        _ => anyhow::bail!("Unsupported architecture: {:?}", target_architecture),
    };

    let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
        ReadVar::from_static(crate::repo_root()),
    );

    let mut pipeline = Pipeline::new();

    let mut job = pipeline.new_job(
        FlowPlatform::host(backend_hint),
        FlowArch::host(backend_hint),
        "build vmm test dependencies",
    );

    job = job.dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init);

    if let (Some(kernel_path), Some(modules_path)) = (
        opts.custom_kernel.clone(),
        opts.custom_kernel_modules.clone(),
    ) {
        job = job.dep_on(
            move |_| flowey_lib_hvlite::_jobs::cfg_versions::Request::LocalKernel {
                arch: recipe_arch,
                kernel: ReadVar::from_static(kernel_path),
                modules: ReadVar::from_static(modules_path),
            },
        );
    }

    // Override UEFI firmware with a local MSVM.fd path
    if let Some(fw_path) = opts.custom_uefi_firmware {
        job = job.dep_on(move |_| {
            flowey_lib_hvlite::_jobs::cfg_versions::Request::LocalUefi(
                recipe_arch,
                ReadVar::from_static(fw_path),
            )
        });
    }

    job = job
        .dep_on(
            |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                hvlite_repo_source: openvmm_repo.clone(),
            },
        )
        .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
            local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                interactive: true,
                auto_install: opts.install_missing_deps,
                ignore_rust_version: true,
            }),
            verbose: ReadVar::from_static(opts.verbose),
            locked: false,
            deny_warnings: false,
            no_incremental: false,
        })
        .dep_on(
            |ctx| flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::Params {
                target,
                test_content_dir: dir,
                selections,
                unstable_whp: opts.unstable_whp,
                release: opts.release,
                build_only: opts.build_only,
                copy_extras: opts.copy_extras,
                custom_kernel_modules: opts.custom_kernel_modules,
                custom_kernel: opts.custom_kernel,
                skip_vhd_prompt: opts.skip_vhd_prompt,
                done: ctx.new_done_handle(),
            },
        );

    job.finish();

    Ok(pipeline)
}
