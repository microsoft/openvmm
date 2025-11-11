// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`BuildIgvmCli`]

use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_hvlite::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use flowey_lib_hvlite::build_openhcl_igvm_from_recipe::OpenhclKernelPackage;
use flowey_lib_hvlite::build_openvmm_hcl::MaxTraceLevel;
use flowey_lib_hvlite::run_cargo_build::common::CommonArch;
use std::path::PathBuf;

#[derive(clap::ValueEnum, Copy, Clone)]
pub enum OpenhclRecipeCli {
    /// Aarch64 OpenHCL
    Aarch64,
    /// Aarch64 OpenHCL, using the dev kernel in VTL2
    Aarch64Devkern,
    /// X64 OpenHCL, with CVM support.
    X64Cvm,
    /// X64 OpenHCL, with CVM support using the dev kernel in VTL2
    X64CvmDevkern,
    /// X64 OpenHCL booting VTL0 using a test linux-direct kernel + initrd (no
    /// UEFI).
    X64TestLinuxDirect,
    /// X64 OpenHCL booting VTL0 using a test linux-direct kernel + initrd (no
    /// UEFI), using the dev kernel in VTL2.
    X64TestLinuxDirectDevkern,
    /// X64 OpenHCL
    X64,
    /// X64 OpenHCL, using the dev kernel in VTL2
    X64Devkern,
}

/// Build OpenHCL IGVM files for local development. DO NOT USE IN CI.
#[derive(clap::Args)]
pub struct BuildIgvmCli<Recipe = OpenhclRecipeCli>
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

    /// Build using release variants of all constituent binary components.
    ///
    /// Uses --profile=boot-release for openhcl_boot, --profile=openhcl-ship
    /// when building openvmm_hcl, etc...
    #[clap(long)]
    pub release: bool,

    /// Configure the IGVM file with the appropriate `-release.json`
    /// manifest variant, and disable debug-only features.
    #[clap(long)]
    pub release_cfg: bool,

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
}

#[derive(clap::Args)]
#[clap(next_help_heading = "Customizations")]
pub struct BuildIgvmCliCustomizations {
    /// Set a custom label for this `build-igvm` invocation. If no label is
    /// provided, customized IGVM files will be output with the label
    /// `{base_recipe_name}-custom`
    #[clap(long, short = 'o')]
    pub build_label: Option<String>,

    /// Override which kernel package to use.
    #[clap(long)]
    pub override_kernel_pkg: Option<KernelPackageKindCli>,

    /// Pass additional features when building openmm_hcl
    #[clap(long)]
    pub override_openvmm_hcl_feature: Vec<String>,

    /// Override architecture used when building. You probably don't want this -
    /// prefer changing the base recipe to something more appropriate.
    #[clap(long)]
    pub override_arch: Option<BuildIgvmArch>,

    /// Override the json manifest passed to igvmfilegen, none means the
    /// debug/release manifest from the base recipe will be used.
    #[clap(long)]
    pub override_manifest: Option<PathBuf>,

    /// Ensure perf tools are included in the release initrd.
    ///
    /// Ensures that openvmm_hcl is not stripped, so that perf tools work
    /// correctly, and requires that the file be built in `--release` mode, so
    /// that perf numbers are more representative of production binaries.
    #[clap(long, requires = "release")]
    pub with_perf_tools: bool,

    /// Preserve debuginfo in the openvmm_hcl binary in the IGVM file.
    ///
    /// This increases the VTL2 memory requirements significantly, and will
    /// likely require passing a `--override-manifest` to compensate.
    #[clap(long)]
    pub with_debuginfo: bool,

    /// Path to custom openvmm_hcl binary, none means openhcl will be built.
    #[clap(long)]
    pub custom_openvmm_hcl: Option<PathBuf>,

    /// Path to custom openhcl_boot, none means the boot loader will be built.
    #[clap(long)]
    pub custom_openhcl_boot: Option<PathBuf>,

    /// Path to custom uefi MSVM.fd, none means the packaged uefi will be used.
    #[clap(long)]
    pub custom_uefi: Option<PathBuf>,

    /// Path to custom kernel vmlinux / Image, none means the packaged kernel
    /// will be used.
    #[clap(long)]
    pub custom_kernel: Option<PathBuf>,

    /// Path to kernel modules, none means the packaged kernel modules will be
    /// used.
    #[clap(long)]
    pub custom_kernel_modules: Option<PathBuf>,

    /// Path to custom vtl0 linux kernel to use if the manifest includes a
    /// direct-boot linux VM.
    ///
    /// If not specified, the packaged openvmm test linux direct kernel is used.
    #[clap(long)]
    pub custom_vtl0_kernel: Option<PathBuf>,

    /// Additional layers to be included in the initrd
    #[clap(long)]
    pub custom_layer: Vec<PathBuf>,

    /// Additional directories to be included in the initrd
    #[clap(long)]
    pub custom_directory: Vec<PathBuf>,

    /// Additional rootfs.config files to use to generate the initrd
    #[clap(long)]
    pub custom_extra_rootfs: Vec<PathBuf>,

    /// (experimental) Include the AP kernel in the IGVM file
    #[clap(long)]
    pub with_sidecar: bool,

    /// (experimental) Path to custom sidecar kernel binary, none means sidecar
    /// will be built.
    #[clap(long, requires = "with_sidecar")]
    pub custom_sidecar: Option<PathBuf>,

    /// The maximum trace level to set for the openvmm_hcl build. Defaults
    /// to `trace` for debug builds and `debug` for release builds.
    #[clap(long)]
    pub max_trace_level: Option<MaxTraceLevelCli>,

    /// (experimental) Only use local dependencies to build. Keeps flowey from
    /// downloading any dependencies from the internet.
    #[clap(long, requires_all = ["custom_openvmm_deps"])]
    pub use_local_deps: bool,

    /// Use a custom openvmm_deps directory.
    #[clap(long)]
    pub custom_openvmm_deps: Option<PathBuf>,
}

#[derive(clap::ValueEnum, Copy, Clone, PartialEq, Eq, Debug)]
pub enum KernelPackageKindCli {
    /// Kernel from the hcl-main branch
    Main,
    /// CVM kernel from the hcl-main branch
    Cvm,
    /// Kernel from the hcl-dev branch
    Dev,
    /// CVM kernel from the hcl-dev brnach
    CvmDev,
}

#[derive(clap::ValueEnum, Copy, Clone, PartialEq, Eq, Debug)]
pub enum MaxTraceLevelCli {
    /// All trace events.
    Trace,
    /// Debug and higher.
    Debug,
    /// Info and higher.
    Info,
    /// Warn and higher.
    Warn,
    /// Error events only.
    Error,
    /// No tracing.
    Off,
}

impl From<MaxTraceLevelCli> for MaxTraceLevel {
    fn from(cli: MaxTraceLevelCli) -> Self {
        match cli {
            MaxTraceLevelCli::Trace => MaxTraceLevel::Trace,
            MaxTraceLevelCli::Debug => MaxTraceLevel::Debug,
            MaxTraceLevelCli::Info => MaxTraceLevel::Info,
            MaxTraceLevelCli::Warn => MaxTraceLevel::Warn,
            MaxTraceLevelCli::Error => MaxTraceLevel::Error,
            MaxTraceLevelCli::Off => MaxTraceLevel::Off,
        }
    }
}

#[derive(clap::ValueEnum, Copy, Clone, PartialEq, Eq, Debug)]
pub enum BuildIgvmArch {
    X86_64,
    Aarch64,
}

pub fn bail_if_running_in_ci() -> anyhow::Result<()> {
    const OVERRIDE_ENV: &str = "I_HAVE_A_GOOD_REASON_TO_RUN_BUILD_IGVM_IN_CI";

    if std::env::var(OVERRIDE_ENV).is_ok() {
        return Ok(());
    }

    for ci_env in ["TF_BUILD", "GITHUB_ACTIONS"] {
        if std::env::var(ci_env).is_ok() {
            log::warn!("Detected that {ci_env} is set");
            log::warn!("");
            log::warn!("Do not use `build-igvm` in CI scripts!");
            log::warn!(
                "This is a local-only, inner-dev-loop tool to build IGVM files, with an UNSTABLE CLI."
            );
            log::warn!("");
            log::warn!(
                "Automated pipelines should use the underlying `flowey` nodes that power build-igvm directly, _without_ relying on its CLI!"
            );
            log::warn!("");
            log::warn!(
                "If you _really_ know what you're doing, you can set {OVERRIDE_ENV} to disable this error."
            );
            anyhow::bail!("attempted to run `build-igvm` in CI")
        }
    }

    Ok(())
}

impl IntoPipeline for BuildIgvmCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("build-igvm is for local use only")
        }

        bail_if_running_in_ci()?;

        let openvmm_repo = flowey_lib_common::git_checkout::RepoSource::ExistingClone(
            ReadVar::from_static(crate::repo_root()),
        );

        let Self {
            recipe,
            release,
            release_cfg,
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
                    max_trace_level,
                    custom_openvmm_deps,
                    use_local_deps,
                },
        } = self;

        if with_perf_tools {
            custom_extra_rootfs.push(crate::repo_root().join("openhcl/perftoolsfs.config"));
        }

        let mut pipeline = Pipeline::new();

        let (pub_out_dir, _) = pipeline.new_artifact("build-igvm");

        // Determine the architecture from the recipe
        let recipe_arch = match recipe {
            OpenhclRecipeCli::X64
            | OpenhclRecipeCli::X64Devkern
            | OpenhclRecipeCli::X64Cvm
            | OpenhclRecipeCli::X64CvmDevkern
            | OpenhclRecipeCli::X64TestLinuxDirect
            | OpenhclRecipeCli::X64TestLinuxDirectDevkern => CommonArch::X86_64,
            OpenhclRecipeCli::Aarch64 | OpenhclRecipeCli::Aarch64Devkern => CommonArch::Aarch64,
        };

        let mut job = pipeline.new_job(
            FlowPlatform::host(backend_hint),
            FlowArch::host(backend_hint),
            "build-igvm",
        );

        job = if use_local_deps {
            let openvmm_deps_path = custom_openvmm_deps.unwrap();
            job.dep_on(move |_| {
                flowey_lib_hvlite::_jobs::cfg_versions::Request::Local(
                    recipe_arch,
                    openvmm_deps_path,
                )
            })
        } else {
            job.dep_on(|_| flowey_lib_hvlite::_jobs::cfg_versions::Request::Download)
        };

        job.dep_on(
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
        .dep_on(|ctx| flowey_lib_hvlite::_jobs::local_build_igvm::Params {
            artifact_dir: ctx.publish_artifact(pub_out_dir),
            done: ctx.new_done_handle(),

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
            release,
            release_cfg,

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
                override_max_trace_level: max_trace_level.map(Into::into),
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
