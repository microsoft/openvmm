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
use flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::BuildSelections;
use flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::VmmTestSelections;
use flowey_lib_hvlite::common::CommonPlatform;
use flowey_lib_hvlite::common::CommonTriple;
use flowey_lib_hvlite::install_vmm_tests_deps::VmmTestsDepSelections;
use flowey_lib_hvlite::install_vmm_tests_deps::VmmTestsDepSelectionsWindows;
use petri_artifacts_core::ArtifactId;
use petri_artifacts_core::ArtifactListOutput;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use vmm_test_images::KnownTestArtifacts;

/// Build and run VMM tests with automatic artifact discovery
#[derive(clap::Args)]
pub struct VmmTestsRunCli {
    /// Specify what target to build the VMM tests for
    ///
    /// If not specified, defaults to the current host target.
    #[clap(long)]
    target: Option<VmmTestTargetCli>,

    /// Directory for the output artifacts.
    ///
    /// If not specified, defaults to `target/vmm_tests`.
    /// WSL-to-Windows runs must override this to a Windows-accessible output
    /// directory (a DrvFs mount like `/mnt/c/...`) only when the selected tests
    /// use disk images that require a Windows filesystem (Hyper-V disks, or
    /// VHDX / dynamic VHD1 images); otherwise the default works.
    #[clap(long)]
    dir: Option<PathBuf>,

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

    /// Download all disk images upfront instead of streaming on demand.
    ///
    /// By default, VHD/ISO disk images are streamed on demand via HTTP
    /// and cached locally, avoiding large upfront downloads. Use this
    /// flag to force all images to be downloaded before tests run.
    #[clap(long)]
    no_lazy_fetch: bool,

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

    /// use the nextest CI profile rather than the default one
    #[clap(long)]
    ci_profile: bool,

    /// Don't reuse prepped vhds, even if they already exist.
    /// Use when making changes to prep_steps
    #[clap(long)]
    no_reuse_prepped_vhds: bool,

    /// Disable secure AVIC support for SNP. This adds the
    /// `disable_secure_avic` cargo feature and sets `secure_avic` to
    /// `disabled` in the IGVM manifest.
    #[clap(long)]
    pub disable_secure_avic: bool,

    /// Run tests inside an emulated incubator.
    ///
    /// Pass `--incubator` on its own to use the default profile for the
    /// selected `--target`, or `--incubator <PATH>` to point at a specific
    /// profile TOML describing the emulated platform (e.g., AArch64 with
    /// SMMUv3).
    ///
    /// When set, `--target` is required and must match the profile's
    /// architecture; artifacts are cross-compiled for that target and tests
    /// run inside the incubator.
    ///
    /// Example: `--incubator --target linux-aarch64-musl`
    #[clap(long, num_args = 0..=1)]
    #[expect(clippy::option_option)]
    incubator: Option<Option<PathBuf>>,
}

struct CargoNextestListRequest<'a> {
    repo_root: &'a Path,
    target: &'a str,
    filter: &'a str,
    release: bool,
    include_ignored: bool,
}

struct RustSuite {
    binary_path: PathBuf,
    testcases: Vec<String>,
}

/// Result of resolving artifact requirements to build/download selections
#[derive(Default, Debug)]
struct ResolvedArtifactSelections {
    /// What to build
    build: BuildSelections,
    /// What to download
    downloads: BTreeSet<KnownTestArtifacts>,
    /// Downloads that must happen even when lazy fetch is enabled (e.g.
    /// VHDs needed by prep_steps, which copies them to create prepped images).
    force_downloads: BTreeSet<KnownTestArtifacts>,
    /// Whether any tests need release IGVM files from GitHub
    needs_release_igvm: bool,
    /// Whether any of the tests require Hyper-V
    needs_hyperv: bool,
    /// Whether any of the tests require hardware isolation
    needs_hardware_isolation: bool,
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
            release,
            build_only,
            copy_extras,
            skip_vhd_prompt,
            no_lazy_fetch,
            custom_kernel_modules,
            custom_kernel,
            custom_uefi_firmware,
            ci_profile,
            no_reuse_prepped_vhds,
            disable_secure_avic,
            incubator,
        } = self;

        // When --incubator is set, --target must also be specified
        // to indicate the cross-compilation target for the incubator.
        if incubator.is_some() && target.is_none() {
            anyhow::bail!("--incubator requires --target (e.g., --target linux-aarch64-musl)");
        }

        let target = resolve_target(target, backend_hint)?;
        let target_os = target.as_triple().operating_system;
        let target_architecture = target.common_arch()?;
        let target_str = target.as_triple().to_string();

        // Windows *guest* payloads (e.g. pipette) must be PE binaries even when
        // the VMM host target is Linux. On a non-WSL Linux build host the MSVC
        // toolchain / Windows SDK is unavailable, so cross-compile those guest
        // binaries with the GNU (mingw-w64) toolchain instead.
        let windows_guest_platform =
            if matches!(FlowPlatform::host(backend_hint), FlowPlatform::Linux(_))
                && !flowey_cli::running_in_wsl()
            {
                CommonPlatform::WindowsGnu
            } else {
                CommonPlatform::WindowsMsvc
            };

        let repo_root = crate::repo_root();

        // Resolve the incubator profile path. `--incubator` with no value uses
        // the default profile for the target; `--incubator <PATH>` overrides.
        let incubator_profile = match incubator {
            None => None,
            Some(Some(path)) => Some(path),
            Some(None) => Some(default_incubator_profile(&repo_root, &target).ok_or_else(
                || {
                    anyhow::anyhow!(
                        "no default incubator profile for target {target_str}; \
                     pass an explicit path with --incubator <PATH>"
                    )
                },
            )?),
        };

        // Artifact discovery only needs to execute the test binary far enough
        // to dump its static artifact metadata (`--list-required-artifacts`),
        // which never boots a VM. So we run it directly rather than through the
        // incubator. The binary is built for the test target, so on a foreign
        // host this relies on the binary being executable (natively, or via
        // binfmt/user-mode emulation).
        //
        // Running outside the real guest means the per-test host capability
        // checks (the source of nextest's `#[ignore]` flag) would wrongly drop
        // incubator tests, so for the incubator path we enumerate ignored tests
        // too — their artifacts still need to be built.
        let include_ignored = build_only || incubator_profile.is_some();

        // Run artifact discovery inline at pipeline construction time since
        // flowey doesn't support conditional requests yet
        log::info!(
            "Discovering artifacts for filter: {} (target: {})",
            filter,
            target
        );

        // Determine which tests match the filter
        let suites = run_cargo_nextest_list(CargoNextestListRequest {
            repo_root: &repo_root,
            target: &target_str,
            filter: &filter,
            release,
            // When using build-only mode, we need to enumerate tests that could be
            // run on any system so that we build all necessary dependencies. By default
            // petri marks incompatible tests as ignored.
            //
            include_ignored,
        })?;

        if suites.is_empty() {
            anyhow::bail!("No tests found for the given filter");
        }

        // Query for the required artifacts
        let mut artifacts = Vec::new();
        for suite in suites.values() {
            artifacts.append(&mut query_test_binary_artifacts(suite)?);
        }

        // Resolve to build selections
        let mut resolved = ResolvedArtifactSelections::default();
        for artifact in artifacts {
            resolved.resolve_artifact(&artifact)?;
        }

        // Determine whether we need hyper-v and/or hardware isolation
        resolved.needs_hyperv = suites
            .values()
            .any(|s| s.testcases.iter().any(|name| name.contains("hyperv")));
        resolved.needs_hardware_isolation = suites.values().any(|s| {
            s.testcases
                .iter()
                .any(|name| name.contains("snp") || name.contains("tdx"))
        });

        // Determine lazy fetch mode.
        //
        // By default, VHD/ISO downloads are skipped and disk images are
        // streamed on demand via HTTP (with local SQLite caching). This
        // avoids multi-GB upfront downloads for dev-inner-loop scenarios.
        //
        // Lazy fetch is disabled for all downloads when the user passes
        // --no-lazy-fetch and for any downloads that are used by a selected
        // Hyper-V test.
        //
        // When both Hyper-V and non-Hyper-V tests are selected, only the
        // artifacts required by Hyper-V tests are downloaded upfront; the
        // rest are lazy-fetched.
        if no_lazy_fetch {
            log::info!("Lazy fetch disabled");
        } else {
            let mut hyperv_tests: usize = 0;
            let mut hyperv_artifacts = Vec::new();
            for (_, suite) in suites.iter() {
                let hyperv_testcases: Vec<_> = suite
                    .testcases
                    .iter()
                    .filter(|name| name.contains("hyperv"))
                    .cloned()
                    .collect();

                if !hyperv_testcases.is_empty() {
                    hyperv_tests += hyperv_testcases.len();
                    hyperv_artifacts.append(&mut query_test_binary_artifacts(&RustSuite {
                        binary_path: suite.binary_path.clone(),
                        testcases: hyperv_testcases,
                    })?);
                }
            }

            resolved.downloads.retain(|a| !a.supports_blob_disk());

            // Re-add force_downloads (prep_steps dependencies) that were removed.
            resolved
                .downloads
                .extend(resolved.force_downloads.iter().cloned());

            if hyperv_tests == 0 {
                log::info!("Lazy fetch enabled: disk images will be streamed on demand via HTTP");
            } else {
                log::info!(
                    "Downloading disk images required by {} Hyper-V tests",
                    hyperv_tests
                );
            }

            // Re-add only the downloads needed for hyper-v. Other selections should
            // remain the same since resolve_artifact can only add selections
            for artifact in hyperv_artifacts {
                resolved.resolve_artifact(&artifact)?;
            }
        }

        log::info!("Resolved selections: {:?}", resolved);

        // Validate the output directory now that we know which disk images the
        // selected tests need. When targeting Windows from WSL, the only hard
        // filesystem constraint is that certain disk images must live on a
        // Windows filesystem rather than a `\\wsl$` 9p path:
        //
        // - Hyper-V tests attach their VHDs to a real Hyper-V VM, whose worker
        //   process can't open disks over 9p, so any Hyper-V disk needs a
        //   Windows path regardless of format.
        // - OpenVMM opens fixed VHD1, VMGS, and raw/ISO images as plain files
        //   (fine over 9p), but routes VHDX (and dynamic/differencing VHD1)
        //   disks through the Windows virtual-disk mount API, which requires a
        //   real local volume. The only such artifact today is the `.vhdx`.
        //
        // All other cases (streamed disks, or fixed-VHD1 files even with
        // `--no-lazy-fetch`) work fine from the default WSL-side directory.
        let needs_windows_disk = !build_only
            && (resolved.needs_hyperv
                || resolved
                    .downloads
                    .iter()
                    .any(|a| a.filename().ends_with(".vhdx")));
        validate_output_dir(dir.as_deref(), target_os, needs_windows_disk)?;
        let test_content_dir = dir.unwrap_or_else(|| repo_root.join("target").join("vmm_tests"));
        std::fs::create_dir_all(&test_content_dir).context("failed to create output directory")?;

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(repo_root),
        );

        let mut pipeline = Pipeline::new();

        let mut job = pipeline.new_job(
            FlowPlatform::host(backend_hint),
            FlowArch::host(backend_hint),
            "build all dependencies and run vmm tests",
        );

        job = job.dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init);

        // Override kernel with local paths if both kernel and modules are specified
        if let (Some(kernel_path), Some(modules_path)) =
            (custom_kernel.clone(), custom_kernel_modules.clone())
        {
            job =
                job.dep_on(
                    move |_| flowey_lib_hvlite::_jobs::cfg_versions::Request::LocalKernel {
                        arch: target_architecture,
                        kernel: ReadVar::from_static(kernel_path),
                        modules: ReadVar::from_static(modules_path),
                    },
                );
        }

        // Override UEFI firmware with a local MSVM.fd path
        if let Some(fw_path) = custom_uefi_firmware {
            job = job.dep_on(move |_| {
                flowey_lib_hvlite::_jobs::cfg_versions::Request::LocalUefi(
                    target_architecture,
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
                    auto_install: install_missing_deps,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(verbose),
                locked: false,
                deny_warnings: false,
                no_incremental: false,
            })
            .dep_on(|ctx| {
                flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::Params {
                    target,
                    windows_guest_platform,
                    test_content_dir,
                    selections: selections_from_resolved(filter, resolved, target_os),
                    release,
                    build_only,
                    copy_extras,
                    custom_kernel_modules,
                    custom_kernel,
                    skip_vhd_prompt,
                    nextest_profile: if ci_profile {
                        flowey_lib_hvlite::run_cargo_nextest_run::NextestProfile::Ci
                    } else {
                        flowey_lib_hvlite::run_cargo_nextest_run::NextestProfile::Default
                    },
                    reuse_prepped_vhds: !no_reuse_prepped_vhds,
                    disable_secure_avic,
                    incubator_profile,
                    done: ctx.new_done_handle(),
                }
            });

        job.finish();

        Ok(pipeline)
    }
}

/// Get test binaries and associated matching tests for a given nextest filter.
// TODO: this function should really be a flowey node without automatic
// dependency installation, but that would require conditional requests.
fn run_cargo_nextest_list<'a>(
    req: CargoNextestListRequest<'a>,
) -> anyhow::Result<BTreeMap<String, RustSuite>> {
    let CargoNextestListRequest {
        repo_root,
        target,
        filter,
        release,
        include_ignored,
    } = req;

    // Check that cargo-nextest is available
    let nextest_check = Command::new("cargo")
        .args(["nextest", "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match nextest_check {
        Ok(status) if status.success() => {}
        _ => anyhow::bail!(
            "cargo-nextest not found. Run 'cargo install --locked cargo-nextest' first."
        ),
    }

    // Step 1: Use nextest to resolve the filter expression to test names and
    // get the binary path
    let mut cmd = Command::new("cargo");
    cmd.stderr(Stdio::inherit());
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
    if include_ignored {
        cmd.args(["--run-ignored", "all"]);
    }
    let nextest_output = cmd.output().context("failed to run cargo nextest list")?;
    anyhow::ensure!(nextest_output.status.success(), "cargo nextest list failed",);
    let nextest_stdout = String::from_utf8(nextest_output.stdout)
        .map_err(|e| anyhow::anyhow!("nextest output is not valid UTF-8: {}", e))?;

    parse_nextest_output(&nextest_stdout)
}

/// Parse `cargo nextest list --message-format json` output to extract test
/// names and binary path.
fn parse_nextest_output(stdout: &str) -> anyhow::Result<BTreeMap<String, RustSuite>> {
    let json: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| anyhow::anyhow!("failed to parse nextest JSON output: {}", e))?;

    let mut suites = BTreeMap::new();

    for (name, suite) in json
        .get("rust-suites")
        .and_then(|s| s.as_object())
        .context("no rust-suites object")?
    {
        let binary_path = PathBuf::from(
            suite
                .get("binary-path")
                .and_then(|v| v.as_str())
                .context("no binary-path str")?,
        );

        let testcases: Vec<_> = suite
            .get("testcases")
            .and_then(|t| t.as_object())
            .context("no testcases object")?
            .iter()
            .filter(|(_, test_info)| {
                test_info
                    .get("filter-match")
                    .and_then(|fm| fm.get("status"))
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| s == "matches")
            })
            .map(|(test_name, _)| test_name.to_owned())
            .collect();

        if !testcases.is_empty() {
            suites.insert(
                name.to_owned(),
                RustSuite {
                    binary_path,
                    testcases,
                },
            );
        }
    }

    Ok(suites)
}

/// Runs the test binary with `--list-required-artifacts --tests-from-stdin`
/// and returns all the required and optional artifacts for all test defined
/// in the RustSuite.
fn query_test_binary_artifacts(suite: &RustSuite) -> anyhow::Result<Vec<String>> {
    log::info!("Using test binary: {}", suite.binary_path.display());
    log::info!("Querying artifacts for {} tests", suite.testcases.len());

    let mut command = Command::new(&suite.binary_path);
    command.arg("--list-required-artifacts");
    command.arg("--tests-from-stdin").stdin(Stdio::piped());

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn test binary")?;

    let stdin_data = suite
        .testcases
        .iter()
        .map(|n| format!("{n}\n"))
        .collect::<String>();
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

    let ArtifactListOutput {
        mut required,
        mut optional,
    } = serde_json::from_str(&artifact_stdout)
        .map_err(|e| anyhow::anyhow!("failed to parse test output JSON: {}", e))?;

    let mut artifacts = Vec::new();
    artifacts.append(&mut required);
    artifacts.append(&mut optional);
    Ok(artifacts)
}

#[derive(clap::ValueEnum, Copy, Clone)]
enum VmmTestTargetCli {
    /// Windows Aarch64
    WindowsAarch64,
    /// Windows X64
    WindowsX64,
    /// Linux X64
    LinuxX64,
    /// Linux Aarch64 (musl, for incubator cross-compilation)
    LinuxAarch64Musl,
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
        VmmTestTargetCli::LinuxAarch64Musl => CommonTriple::AARCH64_LINUX_MUSL,
    })
}

/// Default incubator profile path for a target, used when `--incubator` is
/// passed without an explicit profile path. Returns `None` for targets that
/// have no incubator profile.
fn default_incubator_profile(repo_root: &Path, target: &CommonTriple) -> Option<PathBuf> {
    let name = match *target {
        CommonTriple::AARCH64_LINUX_MUSL => "aarch64-tcg-pcie",
        _ => return None,
    };
    Some(
        repo_root
            .join("petri/incubator/profiles")
            .join(format!("{name}.toml")),
    )
}

/// Validate the output directory path based on the current platform.
///
/// When running under WSL and targeting Windows, some disk images must live on
/// a Windows-accessible path (a DrvFs mount like `/mnt/c/...`) rather than a
/// `\\wsl$` 9p path: Hyper-V disks (attached to a real Hyper-V VM) and VHDX /
/// dynamic VHD1 images (opened via the Windows virtual-disk mount API). This
/// constraint only applies when the selected tests actually use such a disk
/// (`needs_windows_disk`); fixed-VHD1, VMGS, ISO, and streamed disks work fine
/// from the WSL side. On native Windows or Linux this check is a no-op.
fn validate_output_dir(
    dir: Option<&Path>,
    target_os: target_lexicon::OperatingSystem,
    needs_windows_disk: bool,
) -> anyhow::Result<()> {
    if needs_windows_disk
        && flowey_cli::running_in_wsl()
        && matches!(target_os, target_lexicon::OperatingSystem::Windows)
    {
        if let Some(dir) = dir {
            if !flowey_cli::is_wsl_windows_path(dir) {
                anyhow::bail!(
                    "When targeting Windows from WSL, --dir must be a path on Windows \
                        (i.e., on a DrvFs mount like /mnt/c/vmm_tests) because the selected \
                        tests use disk images that require a Windows filesystem (Hyper-V \
                        disks, or VHDX / dynamic VHD1 images). \
                        Got: {}",
                    dir.display()
                );
            }
        } else {
            anyhow::bail!(
                "The selected tests use disk images that require a Windows filesystem \
                    (Hyper-V disks, or VHDX / dynamic VHD1 images) when targeting Windows \
                    from WSL. Specify an output directory on a DrvFs mount with --dir \
                    (e.g., --dir /mnt/c/vmm_tests)."
            )
        }
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
            target_lexicon::OperatingSystem::Windows => {
                VmmTestsDepSelections::Windows(VmmTestsDepSelectionsWindows {
                    hyperv: resolved.needs_hyperv,
                    whp: resolved.build.openvmm,
                    hardware_isolation: resolved.needs_hardware_isolation,
                })
            }
            target_lexicon::OperatingSystem::Linux => VmmTestsDepSelections::Linux,
            _ => unreachable!(),
        },
        needs_release_igvm: resolved.needs_release_igvm,
    }
}

impl ResolvedArtifactSelections {
    /// Resolve a single artifact ID and update selections.
    fn resolve_artifact(&mut self, id: &str) -> anyhow::Result<()> {
        match id {
            // OpenVMM binary
            petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::OPENVMM_LINUX_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_AARCH64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::OPENVMM_LINUX_AARCH64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::OPENVMM_MACOS_AARCH64::GLOBAL_UNIQUE_ID => {
                self.build.openvmm = true;
            }

            // OpenVMM vhost binary (Linux only)
            petri_artifacts_vmm_test::artifacts::OPENVMM_VHOST_LINUX_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::OPENVMM_VHOST_LINUX_AARCH64 ::GLOBAL_UNIQUE_ID => {
                self.build.openvmm_vhost = true;
            }

            // OpenHCL IGVM files
            petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64::GLOBAL_UNIQUE_ID =>
            {
                self.build.openhcl_standard = true;
            }
            petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_DEV_KERNEL_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_DEV_KERNEL_AARCH64::GLOBAL_UNIQUE_ID => {
                self.build.openhcl_standard_dev = true;
            }
            petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_CVM_X64::GLOBAL_UNIQUE_ID
             =>
            {
                self.build.openhcl_cvm = true;
            }
            petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64::GLOBAL_UNIQUE_ID =>
            {
                self.build.openhcl_linux_direct = true;
            }

            // Release IGVM files (downloaded, not built)
            petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_LINUX_DIRECT_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_AARCH64::GLOBAL_UNIQUE_ID =>
            {
                // These are downloaded from GitHub releases, not built
                self.needs_release_igvm = true;
            }

            // Guest test UEFI
            petri_artifacts_vmm_test::artifacts::test_vhd::GUEST_TEST_UEFI_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::test_vhd::GUEST_TEST_UEFI_AARCH64 ::GLOBAL_UNIQUE_ID => {
                self.build.guest_test_uefi = true;
            }

            // TMKs
            petri_artifacts_vmm_test::artifacts::tmks::SIMPLE_TMK_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::tmks::SIMPLE_TMK_AARCH64 ::GLOBAL_UNIQUE_ID => {
                self.build.tmks = true;
            }

            // TMK VMM
            petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_WIN_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_WIN_AARCH64::GLOBAL_UNIQUE_ID => {
                self.build.tmk_vmm_windows = true;
            }
            petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_AARCH64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_X64_MUSL::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_AARCH64_MUSL::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_MACOS_AARCH64::GLOBAL_UNIQUE_ID => {
                self.build.tmk_vmm_linux = true;
            }

            // VmgsTool
            petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_WIN_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_WIN_AARCH64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_LINUX_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_LINUX_AARCH64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_MACOS_AARCH64::GLOBAL_UNIQUE_ID => {
                self.build.vmgstool = true;
            }

            // VmgsTool-Dev
            petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_DEV_WIN_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_DEV_WIN_AARCH64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_DEV_LINUX_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_DEV_LINUX_AARCH64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::vmgstool::VMGSTOOL_DEV_MACOS_AARCH64::GLOBAL_UNIQUE_ID => {
                self.build.vmgstool_dev = true;
            }

            // TPM guest tests
            petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_WINDOWS_X64::GLOBAL_UNIQUE_ID => {
                self.build.tpm_guest_tests_windows = true;
            }
            petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_LINUX_X64::GLOBAL_UNIQUE_ID => {
                self.build.tpm_guest_tests_linux = true;
            }

            // Host tools
            petri_artifacts_vmm_test::artifacts::host_tools::TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64::GLOBAL_UNIQUE_ID =>
            {
                self.build.test_igvm_agent_rpc_server = true;
            }

            // Loadable firmware artifacts (these come from deps, not built)
            petri_artifacts_vmm_test::artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::loadable::LINUX_DIRECT_TEST_INITRD_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::loadable::LINUX_DIRECT_TEST_BZIMAGE_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_AARCH64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::loadable::LINUX_DIRECT_TEST_INITRD_AARCH64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::loadable::PCAT_FIRMWARE_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::loadable::SVGA_FIRMWARE_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::loadable::UEFI_FIRMWARE_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::loadable::UEFI_FIRMWARE_AARCH64::GLOBAL_UNIQUE_ID => {
                // These are resolved from OpenVMM deps, always available
            }

            // Test VHDs
            petri_artifacts_vmm_test::artifacts::test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64::GLOBAL_UNIQUE_ID =>
            {
                self.downloads
                    .insert(KnownTestArtifacts::Gen1WindowsDataCenterCore2022X64Vhd);
            }
            petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64::GLOBAL_UNIQUE_ID =>
            {
                self.downloads
                    .insert(KnownTestArtifacts::Gen2WindowsDataCenterCore2022X64Vhd);
            }
            petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::GLOBAL_UNIQUE_ID =>
            {
                self.downloads
                    .insert(KnownTestArtifacts::Gen2WindowsDataCenterCore2025X64Vhd);
            }
            petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64_PREPPED::GLOBAL_UNIQUE_ID =>
            {
                self.build.prep_steps_standard = true;
                // prep_steps needs actual VHD files on disk to copy them.
                // Force download even when lazy fetch is enabled.
                self.force_downloads
                    .insert(KnownTestArtifacts::Gen2WindowsDataCenterCore2022X64Vhd);
                self.force_downloads
                    .insert(KnownTestArtifacts::Gen2WindowsDataCenterCore2025X64Vhd);
            }
            petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64_NO_VMBUS_PREPPED::GLOBAL_UNIQUE_ID =>
            {
                self.build.prep_steps_no_vmbus = true;
                self.force_downloads
                    .insert(KnownTestArtifacts::Gen2WindowsDataCenterCore2022X64Vhd);
            }
            petri_artifacts_vmm_test::artifacts::test_vhd::FREE_BSD_13_2_X64::GLOBAL_UNIQUE_ID => {
                self.downloads.insert(KnownTestArtifacts::FreeBsd13_2X64Vhd);
            }
            petri_artifacts_vmm_test::artifacts::test_vhd::ALPINE_3_23_X64::GLOBAL_UNIQUE_ID => {
                self.downloads.insert(KnownTestArtifacts::Alpine323X64Vhd);
            }
            petri_artifacts_vmm_test::artifacts::test_vhd::ALPINE_3_23_AARCH64::GLOBAL_UNIQUE_ID => {
                self.downloads
                    .insert(KnownTestArtifacts::Alpine323Aarch64Vhd);
            }
            petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_X64::GLOBAL_UNIQUE_ID => {
                self.downloads
                    .insert(KnownTestArtifacts::Ubuntu2404ServerX64Vhd);
            }
            petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2504_SERVER_X64::GLOBAL_UNIQUE_ID => {
                self.downloads
                    .insert(KnownTestArtifacts::Ubuntu2504ServerX64Vhd);
            }
            petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_AARCH64::GLOBAL_UNIQUE_ID => {
                self.downloads
                    .insert(KnownTestArtifacts::Ubuntu2404ServerAarch64Vhd);
            }
            petri_artifacts_vmm_test::artifacts::test_vhd::WINDOWS_11_ENTERPRISE_AARCH64::GLOBAL_UNIQUE_ID => {
                self.downloads
                    .insert(KnownTestArtifacts::Windows11EnterpriseAarch64Vhdx);
            }

            // Test ISOs (downloaded)
            petri_artifacts_vmm_test::artifacts::test_iso::FREE_BSD_13_2_X64::GLOBAL_UNIQUE_ID => {
                self.downloads.insert(KnownTestArtifacts::FreeBsd13_2X64Iso);
            }

            // Test VMGS files
            petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY::GLOBAL_UNIQUE_ID => {
                self.downloads.insert(KnownTestArtifacts::VmgsWithBootEntry);
            }
            petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_16K_TPM::GLOBAL_UNIQUE_ID => {
                self.downloads.insert(KnownTestArtifacts::VmgsWith16kTpm);
            }

            // OpenHCL usermode binaries (built as part of IGVM)
            petri_artifacts_vmm_test::artifacts::openhcl_igvm::um_bin::LATEST_LINUX_DIRECT_TEST_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_vmm_test::artifacts::openhcl_igvm::um_dbg::LATEST_LINUX_DIRECT_TEST_X64::GLOBAL_UNIQUE_ID =>
            {
                self.build.openhcl_linux_direct = true;
            }

            // Common artifacts (always available, no build needed)
            petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY::GLOBAL_UNIQUE_ID => {}

            // Virtio-win drivers (downloaded from openvmm-deps, always available)
            petri_artifacts_vmm_test::artifacts::virtio_win::VIRTIO_WIN_DRIVERS::GLOBAL_UNIQUE_ID => {}

            // Pipette binaries (from petri_artifacts_common)
            petri_artifacts_common::artifacts::PIPETTE_LINUX_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_common::artifacts::PIPETTE_LINUX_AARCH64::GLOBAL_UNIQUE_ID => {
                self.build.pipette_linux = true;
            }
            petri_artifacts_common::artifacts::PIPETTE_WINDOWS_X64::GLOBAL_UNIQUE_ID
            | petri_artifacts_common::artifacts::PIPETTE_WINDOWS_AARCH64::GLOBAL_UNIQUE_ID => {
                self.build.pipette_windows = true;
            }

            _ => anyhow::bail!("unknown artifact: {id}"),
        };
        Ok(())
    }
}
