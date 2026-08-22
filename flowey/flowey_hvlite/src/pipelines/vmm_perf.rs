// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and run the standalone VMM.Perf harness locally.

use anyhow::Context as _;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::common::CommonArch;
use flowey_lib_hvlite::common::CommonPlatform;
use flowey_lib_hvlite::common::CommonProfile;
use flowey_lib_hvlite::common::CommonTriple;
use flowey_lib_hvlite::run_vmm_perf::VmmPerfProfile;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(clap::ValueEnum, Clone, Copy)]
enum VmmPerfTargetCli {
    LinuxX64Gnu,
    LinuxX64Musl,
}

impl VmmPerfTargetCli {
    fn triple(self) -> CommonTriple {
        CommonTriple::Common {
            arch: CommonArch::X86_64,
            platform: match self {
                Self::LinuxX64Gnu => CommonPlatform::LinuxGnu,
                Self::LinuxX64Musl => CommonPlatform::LinuxMusl,
            },
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum VmmPerfProfileCli {
    BootTime,
    Fio,
    Iperf3,
}

impl From<VmmPerfProfileCli> for VmmPerfProfile {
    fn from(value: VmmPerfProfileCli) -> Self {
        match value {
            VmmPerfProfileCli::BootTime => Self::BootTime,
            VmmPerfProfileCli::Fio => Self::Fio,
            VmmPerfProfileCli::Iperf3 => Self::Iperf3,
        }
    }
}

/// Build and run VMM.Perf without the Petri/nextest test harness.
#[derive(clap::Args)]
pub struct VmmPerfCli {
    /// Linux target for the OpenVMM and VMM.Perf runner binaries.
    #[clap(long, value_enum, default_value = "linux-x64-gnu")]
    target: VmmPerfTargetCli,

    /// Run only the selected profile. May be repeated; defaults to all profiles.
    #[clap(long, value_enum)]
    profile: Vec<VmmPerfProfileCli>,

    /// Root directory for benchmark scratch files and results.
    #[clap(long)]
    dir: Option<PathBuf>,

    /// Replace the default VM matrix with one parameter set.
    #[clap(long = "vmm-perf-vmsizes", value_name = "KEY=VALUE,...")]
    vmm_perf_vm_sizes: Vec<String>,

    /// Override a VMM.Perf parameter on every selected configuration.
    #[clap(long, value_name = "KEY=VALUE")]
    vmm_perf_parameter: Vec<String>,

    /// Release build instead of debug build.
    #[clap(long)]
    release: bool,

    /// Build OpenVMM and the runner without executing benchmarks.
    #[clap(long)]
    build_only: bool,

    /// Automatically install missing build dependencies.
    #[clap(long)]
    install_missing_deps: bool,

    /// Use a local MSVM.fd instead of the configured mu_msvm release.
    #[clap(long)]
    custom_uefi_firmware: Option<PathBuf>,

    /// Enable verbose Flowey output.
    #[clap(long)]
    verbose: bool,
}

impl IntoPipeline for VmmPerfCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("vmm-perf is for local use only")
        }
        if !matches!(FlowPlatform::host(backend_hint), FlowPlatform::Linux(_))
            || !matches!(FlowArch::host(backend_hint), FlowArch::X86_64)
        {
            anyhow::bail!("vmm-perf currently requires a Linux x64 host")
        }

        let Self {
            target,
            profile,
            dir,
            vmm_perf_vm_sizes,
            vmm_perf_parameter,
            release,
            build_only,
            install_missing_deps,
            custom_uefi_firmware,
            verbose,
        } = self;

        let vm_sizes_json = serialize_vm_sizes(&vmm_perf_vm_sizes)?;
        let parameters_json = serialize_parameters(&vmm_perf_parameter)?;
        let profiles = if profile.is_empty() {
            VmmPerfProfile::all()
        } else {
            profile.into_iter().map(Into::into).collect()
        };
        let root_dir = std::path::absolute(
            dir.unwrap_or_else(|| crate::repo_root().join("target").join("vmm_perf")),
        )
        .context("failed to resolve VMM.Perf root directory")?;
        std::fs::create_dir_all(&root_dir)?;

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );
        let mut pipeline = Pipeline::new();
        let mut job = pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "build and run VMM.Perf",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Init);

        if let Some(firmware) = custom_uefi_firmware {
            job = job.dep_on(move |_| {
                flowey_lib_hvlite::_jobs::cfg_versions::Request::LocalUefi(
                    CommonArch::X86_64,
                    ReadVar::from_static(firmware),
                )
            });
        }

        job.dep_on(
            |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                hvlite_repo_source: openvmm_repo,
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
        .dep_on(
            |ctx| flowey_lib_hvlite::_jobs::local_build_and_run_vmm_perf::Params {
                target: target.triple(),
                profile: CommonProfile::from_release(release),
                root_dir,
                profiles,
                vm_sizes_json,
                parameters_json,
                build_only,
                done: ctx.new_done_handle(),
            },
        )
        .finish();

        Ok(pipeline)
    }
}

fn serialize_vm_sizes(vm_sizes: &[String]) -> anyhow::Result<Option<String>> {
    if vm_sizes.is_empty() {
        return Ok(None);
    }

    let mut parsed = Vec::with_capacity(vm_sizes.len());
    for (index, vm_size) in vm_sizes.iter().enumerate() {
        let parameters = parse_parameter_set(
            vm_size.split(','),
            &format!("--vmm-perf-vmsizes at position {}", index + 1),
        )?;
        parsed.push(serde_json::json!({ "parameters": parameters }));
    }
    Ok(Some(serde_json::to_string(&parsed)?))
}

fn serialize_parameters(parameters: &[String]) -> anyhow::Result<Option<String>> {
    if parameters.is_empty() {
        return Ok(None);
    }
    let parameters = parse_parameter_set(
        parameters.iter().map(String::as_str),
        "--vmm-perf-parameter",
    )?;
    Ok(Some(serde_json::to_string(&parameters)?))
}

fn parse_parameter_set<'a>(
    values: impl IntoIterator<Item = &'a str>,
    context: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut parameters = BTreeMap::new();
    for value in values {
        let (name, value) = value
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("{context} value {value:?} must use KEY=VALUE"))?;
        let name = name.trim();
        let value = value.trim();
        anyhow::ensure!(
            !name.is_empty(),
            "{context} contains an empty parameter name"
        );
        anyhow::ensure!(
            parameters
                .insert(name.to_owned(), value.to_owned())
                .is_none(),
            "{context} contains duplicate parameter {name:?}"
        );
    }
    anyhow::ensure!(!parameters.is_empty(), "{context} cannot be empty");
    Ok(parameters)
}
