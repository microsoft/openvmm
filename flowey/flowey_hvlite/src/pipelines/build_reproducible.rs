// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! See [`BuildReproducibleCli`]

use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::node::prelude::ReadVar;
use flowey::pipeline::prelude::*;
use flowey_lib_common::git_checkout::RepoSource;
use flowey_lib_hvlite::_jobs::build_and_publish_openhcl_igvm_from_recipe::OpenhclIgvmBuildParams;
use flowey_lib_hvlite::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use flowey_lib_hvlite::build_openvmm_hcl::OpenvmmHclBuildProfile;
use flowey_lib_hvlite::resolve_openhcl_kernel_package::OpenhclKernelPackageKind;
use flowey_lib_hvlite::run_cargo_build::common::CommonArch;
use flowey_lib_hvlite::run_cargo_build::common::CommonPlatform;
use flowey_lib_hvlite::run_cargo_build::common::CommonTriple;
use target_lexicon::Triple;

/// A list of pre-defined OpenHCL recipes that support being built reproducibly. Each recipe has a matching CI pipeline job that can be reproduced with this local CLI.
#[derive(clap::ValueEnum, Copy, Clone)]
pub enum ReproducibleOpenHclRecipe {
    X64Cvm,
}

/// Build reproducible artifacts locally. DO NOT USE IN CI.
#[derive(clap::Args)]
pub struct BuildReproducibleCli {
    /// Specify which OpenHCL recipe to build / customize off-of.
    ///
    /// A "recipe" corresponds to the various standard IGVM SKUs that are
    /// actively supported and tested in our build infrastructure.
    ///
    /// It encodes all the details of what goes into an individual IGVM file,
    /// such as what build flags `openvmm_hcl` should be built with, what goes
    /// into a VTL2 initrd, what `igvmfilegen` manifest is being used, etc...
    pub recipe: ReproducibleOpenHclRecipe,

    /// Build using release variants of all constituent binary components.
    ///
    /// Uses --profile=boot-release for openhcl_boot, --profile=openhcl-ship
    /// when building openvmm_hcl, etc...
    #[clap(long)]
    pub release: bool,
}

impl IntoPipeline for BuildReproducibleCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("build-reproducible is for local use only")
        }

        let Self { recipe, release } = self;

        let mut pipeline = Pipeline::new();

        let (pub_openhcl_igvm, _use_openhcl_igvm) = pipeline.new_artifact("x64-cvm-openhcl-igvm");
        let (pub_openhcl_igvm_extras, _use_openhcl_igvm_extras) =
            pipeline.new_artifact("x64-cvm-openhcl-igvm-extras");

        let cfg_common_params = crate::pipelines_shared::cfg_common_params::get_cfg_common_params(
            &mut pipeline,
            backend_hint,
            None,
        )?;

        let openvmm_repo_source =
            RepoSource::ExistingClone(ReadVar::from_static(crate::repo_root()));

        let mut job = pipeline.new_job(
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Nix),
            FlowArch::host(backend_hint),
            "build-reproducible",
        );

        // wrap all shell commands with `nix-shell --pure --run`
        job = job.set_command_wrapper(flowey::shell::CommandWrapperKind::NixShell {
            path: Some(crate::repo_root().join("shell.nix")),
        });

        let openvmm_hcl_profile = if release {
            OpenvmmHclBuildProfile::OpenvmmHclShip
        } else {
            OpenvmmHclBuildProfile::Debug
        };

        let (recipe_arch, kernel_kind) = match recipe {
            ReproducibleOpenHclRecipe::X64Cvm => {
                (CommonArch::X86_64, OpenhclKernelPackageKind::Cvm)
            }
        };

        let igvm_file = match recipe {
            ReproducibleOpenHclRecipe::X64Cvm => OpenhclIgvmRecipe::X64Cvm,
        };

        let openhcl_musl_target = |arch: CommonArch| -> Triple {
            CommonTriple::Common {
                arch,
                platform: CommonPlatform::LinuxMusl,
            }
            .as_triple()
        };

        job = job.dep_on(&cfg_common_params);
        job = job.dep_on(
            |_| flowey_lib_hvlite::_jobs::cfg_hvlite_reposource::Params {
                hvlite_repo_source: openvmm_repo_source.clone(),
            },
        );
        job = job.dep_on(|ctx| {
            flowey_lib_hvlite::_jobs::build_openhcl_igvm_from_recipe_nix::Params {
                arch: recipe_arch,
                kernel_kind,
                igvm_files: vec![igvm_file]
                    .into_iter()
                    .map(|recipe| OpenhclIgvmBuildParams {
                        profile: openvmm_hcl_profile,
                        recipe,
                        custom_target: Some(CommonTriple::Custom(openhcl_musl_target(recipe_arch))),
                    })
                    .collect(),
                artifact_dir_openhcl_igvm: ctx.publish_artifact(pub_openhcl_igvm),
                artifact_dir_openhcl_igvm_extras: ctx.publish_artifact(pub_openhcl_igvm_extras),
                artifact_openhcl_verify_size_baseline: None,
                done: ctx.new_done_handle(),
            }
        });

        job.finish();

        Ok(pipeline)
    }
}
