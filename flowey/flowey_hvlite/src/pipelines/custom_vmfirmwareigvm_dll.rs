// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`CustomVmfirmwareigvmDllCli`]

use crate::pipelines_shared::cfg_common_params::CommonArchCli;
use anyhow::Context;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use std::path::PathBuf;

/// Encapsulate an existing pre-built IGVM file into *unsigned*
/// `vmfirmwareigvm.dll` resource DLL.
///
/// Unlike `build-igvm`, this tool will NOT build OpenHCL from scratch. This
/// tool streamlines the process of building the in-tree `vmfirmwareigvm_dll`
/// crate (which requires setting various env vars, installing certain
/// dependencies, etc...).
///
/// NOTE: This tool is primarily intended for use by Microsoft employees, as
/// open-source deployments of OpenHCL typically load the IGVM file directly
/// (rather than being encapsulated in a resource-DLL).
#[derive(clap::Args)]
pub struct CustomVmfirmwareigvmDllCli {
    /// Path to IGVM payload to encapsulate in the vmfirmwareigvm resource DLL.
    pub igvm_payload: PathBuf,

    /// Architecture the DLL should be built for.
    ///
    /// Defaults to the current host architecture.
    #[clap(long)]
    pub arch: Option<CommonArchCli>,
}

impl IntoPipeline for CustomVmfirmwareigvmDllCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("build-igvm is for local use only")
        }

        // DEVNOTE: it would be nice to figure out what sort of magic is
        // required for the WSL2 case to work. The tricky part is dealing with
        // the underlying invocations to `rc.exe` via WSL2.
        if !matches!(FlowPlatform::host(backend_hint), FlowPlatform::Windows) {
            anyhow::bail!("custom-vmfirmwareigvm-dll only runs on Windows (WSL2 is NOT supported)")
        }

        let CustomVmfirmwareigvmDllCli { arch, igvm_payload } = self;

        let arch = match arch {
            Some(arch) => arch,
            None => FlowArch::host(backend_hint).try_into()?,
        };
        let igvm_payload = std::path::absolute(igvm_payload)
            .context("could not make path to igvm payload absolute")?;

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );

        let mut pipeline = Pipeline::new();

        let (pub_out_dir, _) = pipeline.new_artifact("custom-vmfirmwareigvm-dll");

        pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "custom-vmfirmwareigvm-dll",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Download)
            .dep_on(
                |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                    hvlite_repo_source: openvmm_repo,
                },
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: false,
                    force_nuget_mono: false, // no oss nuget packages
                    external_nuget_auth: false,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(false),
                locked: false,
                deny_warnings: false,
            })
            .dep_on(
                |ctx| flowey_lib_hvlite::_jobs::local_custom_vmfirmwareigvm_dll::Params {
                    arch: arch.into(),
                    igvm_payload,
                    artifact_dir: ctx.publish_artifact(pub_out_dir),
                    done: ctx.new_done_handle(),
                },
            )
            .finish();

        Ok(pipeline)
    }
}
