// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::VmmTestSelectionFlags;
use flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::VmmTestSelections;
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
            flags,
            verbose,
            install_missing_deps,
            unstable_whp,
            release,
            build_only,
            copy_extras,
            custom_kernel_modules,
            custom_kernel,
        } = self;

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );

        let mut pipeline = Pipeline::new();

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

        let target = match target {
            VmmTestTargetCli::WindowsAarch64 => CommonTriple::AARCH64_WINDOWS_MSVC,
            VmmTestTargetCli::WindowsX64 => CommonTriple::X86_64_WINDOWS_MSVC,
            VmmTestTargetCli::LinuxX64 => CommonTriple::X86_64_LINUX_GNU,
        };
        let target_os = target.as_triple().operating_system;
        let target_architecture = target.as_triple().architecture;

        let recipe_arch = match target_architecture {
            target_lexicon::Architecture::X86_64 => CommonArch::X86_64,
            target_lexicon::Architecture::Aarch64(_) => CommonArch::Aarch64,
            _ => anyhow::bail!("Unsupported architecture: {:?}", target_architecture),
        };

        let mut job = pipeline.new_job(
            FlowPlatform::host(backend_hint),
            FlowArch::host(backend_hint),
            "build vmm test dependencies",
        );

        job = job.dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init);

        // Override kernel with local paths if both kernel and modules are specified
        if let (Some(kernel_path), Some(modules_path)) =
            (custom_kernel.clone(), custom_kernel_modules.clone())
        {
            job =
                job.dep_on(
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
                    auto_install: install_missing_deps,
                    force_nuget_mono: false,
                    external_nuget_auth: false,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(verbose),
                locked: false,
                deny_warnings: false,
            })
            .dep_on(|ctx| {
                flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::Params {
                    target,
                    test_content_dir: dir,
                    selections: if let Some(filter) = filter {
                        VmmTestSelections::Custom {
                            filter,
                            artifacts,
                            // TODO: add a way to manually specify these
                            // For now, just build and install everything.
                            build: Default::default(),
                            deps: match target_os {
                                target_lexicon::OperatingSystem::Windows => {
                                    VmmTestsDepSelections::Windows {
                                        hyperv: true,
                                        whp: true,
                                        // No hardware isolation support on Aarch64, so don't default to needing it when the
                                        // user specifies a custom filter.
                                        hardware_isolation: match target_architecture {
                                            target_lexicon::Architecture::Aarch64(_) => false,
                                            target_lexicon::Architecture::X86_64 => true,
                                            _ => panic!(
                                                "Unhandled architecture: {:?}",
                                                target_architecture
                                            ),
                                        },
                                    }
                                }
                                target_lexicon::OperatingSystem::Linux => {
                                    VmmTestsDepSelections::Linux
                                }
                                _ => unreachable!(),
                            },
                        }
                    } else {
                        VmmTestSelections::Flags(flags.unwrap_or_default())
                    },
                    unstable_whp,
                    release,
                    build_only,
                    copy_extras,
                    custom_kernel_modules,
                    custom_kernel,
                    done: ctx.new_done_handle(),
                }
            });

        job.finish();

        Ok(pipeline)
    }
}
