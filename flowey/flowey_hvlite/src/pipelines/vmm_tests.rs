// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context as _;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::BuildSelections;
use flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::VmmTestSelectionFlags;
use flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::VmmTestSelections;
use flowey_lib_hvlite::artifact_to_build_mapping::ResolvedArtifactSelections;
use flowey_lib_hvlite::install_vmm_tests_deps::VmmTestsDepSelections;
use flowey_lib_hvlite::run_cargo_build::common::CommonArch;
use flowey_lib_hvlite::run_cargo_build::common::CommonTriple;
use std::path::PathBuf;
use vmm_test_images::KnownTestArtifacts;

#[derive(clap::ValueEnum, Copy, Clone)]
pub enum VmmTestTargetCli {
    /// Windows Aarch64
    WindowsAarch64,
    /// Windows X64
    WindowsX64,
    /// Linux X64
    LinuxX64,
}

/// Resolve a CLI target option to a CommonTriple, defaulting to the host.
pub(crate) fn resolve_target(
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

/// Validate that the output directory is a Windows path when targeting Windows
/// from WSL.
pub(crate) fn validate_wsl_dir(
    dir: &std::path::Path,
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

/// Resolve `ResolvedArtifactSelections` to `VmmTestSelections::Custom`.
pub(crate) fn selections_from_resolved(
    filter: String,
    resolved: ResolvedArtifactSelections,
    target_os: target_lexicon::OperatingSystem,
) -> VmmTestSelections {
    VmmTestSelections::Custom {
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

/// Options for building and running VMM tests, shared between `vmm-tests` and
/// `vmm-tests-run`.
pub(crate) struct VmmTestsPipelineOptions {
    pub verbose: bool,
    pub install_missing_deps: bool,
    pub unstable_whp: bool,
    pub release: bool,
    pub build_only: bool,
    pub copy_extras: bool,
    pub skip_vhd_prompt: bool,
    pub custom_kernel_modules: Option<PathBuf>,
    pub custom_kernel: Option<PathBuf>,
}

/// Construct the pipeline job for building and running VMM tests.
///
/// This is the shared pipeline construction logic used by both `vmm-tests` and
/// `vmm-tests-run`.
pub(crate) fn build_vmm_tests_pipeline(
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

/// Build everything needed and run the VMM tests
#[derive(clap::Args)]
pub struct VmmTestsCli {
    /// Specify what target to build the VMM tests for
    ///
    /// If not specified, defaults to the current host target.
    #[clap(long)]
    target: Option<VmmTestTargetCli>,

    /// Directory for the output artifacts
    #[clap(long)]
    dir: PathBuf,

    /// Custom test filter
    #[clap(long, conflicts_with("flags"))]
    filter: Option<String>,
    /// Custom list of artifacts to download
    #[clap(long, conflicts_with("flags"), requires("filter"))]
    artifacts: Vec<KnownTestArtifacts>,
    /// Path to a JSON file containing discovered artifacts (from vmm-tests-discover).
    /// This enables building only the dependencies needed for the specified tests.
    #[clap(long, conflicts_with_all(["flags", "artifacts"]), requires("filter"))]
    artifacts_file: Option<PathBuf>,
    /// Flags used to generate the VMM test filter
    ///
    /// Syntax: `--flags=<+|-><flag>,..`
    ///
    /// Available flags with default values:
    ///
    /// `-tdx,-snp,-hyperv_vbs,+windows,+ubuntu,+freebsd,+linux,+openhcl,+openvmm,+hyperv,+uefi,+pcat,+tmk,+guest_test_uefi`
    // TODO: Automatically generate the list of possible flags
    #[clap(long)]
    flags: Option<VmmTestSelectionFlags>,

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

impl IntoPipeline for VmmTestsCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("vmm-tests is for local use only")
        }

        let Self {
            target,
            dir,
            filter,
            artifacts,
            artifacts_file,
            flags,
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

        let target = resolve_target(target, backend_hint)?;
        let target_os = target.as_triple().operating_system;
        let target_architecture = target.as_triple().architecture;

        // Handle artifacts-file mode: read discovered artifacts from JSON file
        let using_artifacts_file = artifacts_file.is_some();
        let (resolved_filter, resolved_artifacts, resolved_build, needs_release_igvm) =
            if let Some(artifacts_path) = artifacts_file {
                let filter = filter.expect("--filter is required with --artifacts-file");
                log::info!(
                    "Reading discovered artifacts from: {}",
                    artifacts_path.display()
                );

                let json_output = std::fs::read_to_string(&artifacts_path).with_context(|| {
                    format!(
                        "failed to read artifacts file: {}",
                        artifacts_path.display()
                    )
                })?;

                // Parse the JSON and resolve to build selections
                let resolved = ResolvedArtifactSelections::from_artifact_list_json(
                    &json_output,
                    target_architecture,
                    target_os,
                )
                .context("failed to parse artifact list")?;

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

                (
                    filter,
                    resolved.downloads.into_iter().collect(),
                    resolved.build,
                    resolved.needs_release_igvm,
                )
            } else if let Some(filter) = filter {
                // Custom mode with explicit artifacts
                (filter, artifacts, BuildSelections::default(), true)
            } else {
                // Flags mode - not using artifacts-file, return early to use existing logic
                (String::new(), Vec::new(), BuildSelections::default(), true)
            };

        validate_wsl_dir(&dir, target_os)?;

        // Determine test selections based on mode
        let selections = if using_artifacts_file {
            selections_from_resolved(
                resolved_filter,
                ResolvedArtifactSelections {
                    build: resolved_build,
                    downloads: resolved_artifacts.into_iter().collect(),
                    unknown: Vec::new(),
                    target_from_file: None,
                    needs_release_igvm,
                },
                target_os,
            )
        } else if !resolved_filter.is_empty() {
            // Custom mode with explicit artifacts (filter was specified without artifacts-file)
            VmmTestSelections::Custom {
                filter: resolved_filter,
                artifacts: resolved_artifacts,
                build: BuildSelections::default(),
                deps: match target_os {
                    target_lexicon::OperatingSystem::Windows => VmmTestsDepSelections::Windows {
                        hyperv: true,
                        whp: true,
                        hardware_isolation: match target_architecture {
                            target_lexicon::Architecture::Aarch64(_) => false,
                            target_lexicon::Architecture::X86_64 => true,
                            _ => panic!("Unhandled architecture: {:?}", target_architecture),
                        },
                    },
                    target_lexicon::OperatingSystem::Linux => VmmTestsDepSelections::Linux,
                    _ => unreachable!(),
                },
                needs_release_igvm,
            }
        } else {
            // Flags mode
            VmmTestSelections::Flags(flags.unwrap_or_default())
        };

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
