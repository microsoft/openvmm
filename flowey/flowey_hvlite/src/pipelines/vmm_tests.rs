// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::BuildSelections;
use flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::VmmTestSelections;
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

/// Flags used to generate the VMM test filter
#[derive(clap::Args)]
#[clap(next_help_heading = "Test Selections")]
pub struct VmmTestSelectionsCli {
    /// Enable Hyper-V TDX tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    tdx: bool,
    /// Enable Hyper-V VBS tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    hyperv_vbs: bool,
    /// Skip Windows guest tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    no_windows: bool,
    /// Skip Ubuntu guest tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    no_ubuntu: bool,
    /// Skip FreeBSD guest tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    no_freebsd: bool,
    /// Skip OpenHCL tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    no_openhcl: bool,
    /// Skip OpenVMM tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    no_openvmm: bool,
    /// Skip Hyper-V tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    no_hyperv: bool,
    /// Skip UEFI tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    no_uefi: bool,
    /// Skip PCAT tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    no_pcat: bool,
    /// Skip TMK tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    no_tmk: bool,
    /// Skip guest test uefi tests
    #[clap(long, conflicts_with_all(&["filter", "artifacts"]))]
    no_guest_test_uefi: bool,
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
    dir: Option<PathBuf>,

    /// Custom test filter
    #[clap(long)]
    filter: Option<String>,
    /// Custom list of artifacts to download
    #[clap(long)]
    artifacts: Vec<KnownTestArtifacts>,
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

    #[clap(flatten)]
    selections: VmmTestSelectionsCli,
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
            verbose,
            install_missing_deps,
            unstable_whp,
            release,
            build_only,
            copy_extras,
            selections:
                VmmTestSelectionsCli {
                    tdx,
                    hyperv_vbs,
                    no_windows,
                    no_ubuntu,
                    no_freebsd,
                    no_openhcl,
                    no_openvmm,
                    no_hyperv,
                    no_uefi,
                    no_pcat,
                    no_tmk,
                    no_guest_test_uefi,
                },
        } = self;

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );

        let mut pipeline = Pipeline::new();

        let host_target = match (
            FlowArch::host(backend_hint),
            FlowPlatform::host(backend_hint),
        ) {
            (FlowArch::Aarch64, FlowPlatform::Windows) => VmmTestTargetCli::WindowsAarch64,
            (FlowArch::X86_64, FlowPlatform::Windows) => VmmTestTargetCli::WindowsX64,
            (FlowArch::X86_64, FlowPlatform::Linux(_)) => VmmTestTargetCli::LinuxX64,
            _ => anyhow::bail!("unsupported host"),
        };

        let target = match target.unwrap_or(host_target) {
            VmmTestTargetCli::WindowsAarch64 => CommonTriple::AARCH64_WINDOWS_MSVC,
            VmmTestTargetCli::WindowsX64 => CommonTriple::X86_64_WINDOWS_MSVC,
            VmmTestTargetCli::LinuxX64 => CommonTriple::X86_64_LINUX_GNU,
        };

        pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "build vmm test dependencies",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request {})
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
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::local_build_and_run_nextest_vmm_tests::Params {
                    target,
                    test_content_dir: dir,
                    selections: if let Some(filter) = filter {
                        VmmTestSelections::Custom {
                            filter,
                            artifacts,
                            build: BuildSelections::default(),
                        }
                    } else {
                        VmmTestSelections::Flags {
                            tdx,
                            hyperv_vbs,
                            no_windows,
                            no_ubuntu,
                            no_freebsd,
                            no_openhcl,
                            no_openvmm,
                            no_hyperv,
                            no_uefi,
                            no_pcat,
                            no_tmk,
                            no_guest_test_uefi,
                        }
                    },
                    unstable_whp,
                    release,
                    build_only,
                    copy_extras,
                    done: ctx.new_done_handle(),
                },
            )
            .finish();

        Ok(pipeline)
    }
}
