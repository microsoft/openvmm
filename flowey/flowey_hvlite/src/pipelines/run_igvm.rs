// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::build_igvm::bail_if_running_in_ci;
use super::build_igvm::BuildIgvmArch;
use super::build_igvm::BuildIgvmCliCustomizations;
use super::build_igvm::KernelPackageKindCli;
use super::build_igvm::OpenhclRecipeCli;
use flowey::node::prelude::FlowArch;
use flowey::node::prelude::FlowPlatform;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::HostExt;
use flowey::pipeline::prelude::IntoPipeline;
use flowey::pipeline::prelude::Pipeline;
use flowey::pipeline::prelude::PipelineBackendHint;
use flowey_lib_hvlite::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use flowey_lib_hvlite::build_openhcl_igvm_from_recipe::OpenhclKernelPackage;
use flowey_lib_hvlite::run_cargo_build::common::CommonArch;

/// Run OpenHCL IGVM files for local development. DO NOT USE IN CI.
#[derive(clap::Args)]
pub struct RunIgvmCli<Recipe = OpenhclRecipeCli>
where
    // Make the recipe generic so that out-of-tree flowey implementations can
    // slot in a custom set of recipes to build with.
    Recipe: clap::ValueEnum + Clone + Send + Sync + 'static,
{
    /// Specify which OpenHCL recipe to build / customize off-of.
    ///
    /// A "recipe" corresponds to the various standard IGVM SKUs that are
    /// actively supported and tested in our build infrastructure.
    ///
    /// It encodes all the details of what goes into an individual IGVM file,
    /// such as what build flags `openvmm_hcl` should be built with, what goes
    /// into a VTL2 initrd, what `igvmfilegen` manifest is being used, etc...
    pub recipe: Recipe,

    /// Build using release variants of all constituent components.
    ///
    /// Uses --profile=boot-release for openhcl_boot, --profile=openhcl-ship
    /// when building openvmm_hcl, `--min-interactive` vtl2 initrd
    /// configuration, `-release.json` manifest variant, etc...
    #[clap(long)]
    pub release: bool,

    /// pass `--verbose` to cargo
    #[clap(long)]
    pub verbose: bool,

    /// pass `--locked` to cargo
    #[clap(long)]
    pub locked: bool,

    /// Automatically install any missing required dependencies.
    #[clap(long)]
    pub install_missing_deps: bool,

    #[clap(flatten)]
    pub customizations: BuildIgvmCliCustomizations,

    /// Additional parameters to pass to OpenVMM
    #[clap(trailing_var_arg = true)]
    pub trailing_args: Vec<String>,
}

impl IntoPipeline for RunIgvmCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("run-igvm is for local use only")
        }

        bail_if_running_in_ci()?;

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );

        let Self {
            trailing_args,
            recipe,
            release,
            verbose,
            locked,
            install_missing_deps,
            customizations:
                BuildIgvmCliCustomizations {
                    build_label,
                    override_kernel_pkg,
                    override_openvmm_hcl_feature,
                    override_arch,
                    override_manifest,
                    with_perf_tools,
                    with_debuginfo,
                    custom_openvmm_hcl,
                    custom_openhcl_boot,
                    custom_uefi,
                    custom_kernel,
                    custom_kernel_modules,
                    custom_vtl0_kernel,
                    custom_layer,
                    custom_directory,
                    with_sidecar,
                    custom_sidecar,
                    mut custom_extra_rootfs,
                },
        } = self;

        if with_perf_tools {
            custom_extra_rootfs.push(
                crate::repo_root()
                    .join("openhcl/perftoolsfs.config")
                    .clone(),
            );
        }

        let mut pipeline = Pipeline::new();

        let (pub_out_dir, _) = pipeline.new_artifact("build-igvm");

        // Build OpenHCL
        pipeline
            .new_job(
                FlowPlatform::host(backend_hint),
                FlowArch::host(backend_hint),
                "build-igvm",
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request {})
            .dep_on(
                |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                    hvlite_repo_source: openvmm_repo,
                },
            )
            .dep_on(|_| flowey_lib_hvlite::_jobs::cfg_common::Params {
                local_only: Some(flowey_lib_hvlite::_jobs::cfg_common::LocalOnlyParams {
                    interactive: true,
                    auto_install: install_missing_deps,
                    force_nuget_mono: false, // no oss nuget packages
                    external_nuget_auth: false,
                    ignore_rust_version: true,
                }),
                verbose: ReadVar::from_static(verbose),
                locked,
                deny_warnings: false,
            })
            .dep_on(|ctx| flowey_lib_hvlite::_jobs::local_run_igvm::Params {
                release,
                openvmm_args: trailing_args,
                base_recipe: match recipe {
                    OpenhclRecipeCli::X64 => OpenhclIgvmRecipe::X64,
                    OpenhclRecipeCli::X64Devkern => OpenhclIgvmRecipe::X64Devkern,
                    OpenhclRecipeCli::X64TestLinuxDirect => OpenhclIgvmRecipe::X64TestLinuxDirect,
                    OpenhclRecipeCli::X64TestLinuxDirectDevkern => {
                        OpenhclIgvmRecipe::X64TestLinuxDirectDevkern
                    }
                    OpenhclRecipeCli::X64Cvm => OpenhclIgvmRecipe::X64Cvm,
                    OpenhclRecipeCli::X64CvmDevkern => OpenhclIgvmRecipe::X64CvmDevkern,
                    OpenhclRecipeCli::Aarch64 => OpenhclIgvmRecipe::Aarch64,
                    OpenhclRecipeCli::Aarch64Devkern => OpenhclIgvmRecipe::Aarch64Devkern,
                },
                artifact_dir: ctx.publish_artifact(pub_out_dir),
                done: ctx.new_done_handle(),
                customizations: flowey_lib_hvlite::_jobs::local_build_igvm::Customizations {
                    build_label,
                    override_arch: override_arch.map(|a| match a {
                        BuildIgvmArch::X86_64 => CommonArch::X86_64,
                        BuildIgvmArch::Aarch64 => CommonArch::Aarch64,
                    }),
                    with_perf_tools,
                    with_debuginfo,
                    override_kernel_pkg: override_kernel_pkg.map(|p| match p {
                        KernelPackageKindCli::Main => OpenhclKernelPackage::Main,
                        KernelPackageKindCli::Cvm => OpenhclKernelPackage::Cvm,
                        KernelPackageKindCli::Dev => OpenhclKernelPackage::Dev,
                        KernelPackageKindCli::CvmDev => OpenhclKernelPackage::CvmDev,
                    }),
                    with_sidecar,
                    custom_extra_rootfs,
                    override_openvmm_hcl_feature,
                    custom_sidecar,
                    override_manifest,
                    custom_openvmm_hcl,
                    custom_openhcl_boot,
                    custom_uefi,
                    custom_kernel,
                    custom_kernel_modules,
                    custom_vtl0_kernel,
                    custom_layer,
                    custom_directory,
                },
            })
            .finish();

        Ok(pipeline)
    }
}
