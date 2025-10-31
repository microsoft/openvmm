// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local-only job that supports the `cargo xflowey build-igvm` CLI

use flowey::node::prelude::*;

use crate::build_openhcl_boot::OpenhclBootOutput;
use crate::build_openhcl_igvm_from_recipe::IgvmManifestPath;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipeDetails;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipeDetailsLocalOnly;
use crate::build_openhcl_igvm_from_recipe::OpenhclKernelPackage;
use crate::build_openhcl_igvm_from_recipe::Vtl0KernelType;
use crate::build_openhcl_initrd::OpenhclInitrdExtraParams;
use crate::build_openvmm_hcl::MaxTraceLevel;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::build_openvmm_hcl::OpenvmmHclFeature;
use crate::build_openvmm_hcl::OpenvmmHclOutput;
use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonTriple;
use crate::run_igvmfilegen::IgvmOutput;

#[derive(Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Customizations {
    pub build_label: Option<String>,
    pub custom_directory: Vec<PathBuf>,
    pub custom_kernel_modules: Option<PathBuf>,
    pub custom_kernel: Option<PathBuf>,
    pub custom_layer: Vec<PathBuf>,
    pub custom_openhcl_boot: Option<PathBuf>,
    pub custom_openvmm_hcl: Option<PathBuf>,
    pub custom_sidecar: Option<PathBuf>,
    pub custom_uefi: Option<PathBuf>,
    pub custom_vtl0_kernel: Option<PathBuf>,
    pub custom_extra_rootfs: Vec<PathBuf>,
    pub override_arch: Option<CommonArch>,
    pub override_kernel_pkg: Option<OpenhclKernelPackage>,
    pub override_manifest: Option<PathBuf>,
    pub override_openvmm_hcl_feature: Vec<String>,
    pub override_max_trace_level: Option<MaxTraceLevel>,
    pub with_debuginfo: bool,
    pub with_perf_tools: bool,
    pub with_sidecar: bool,
}

flowey_request! {
    pub struct Params {
        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,

        pub base_recipe: OpenhclIgvmRecipe,
        pub release: bool,
        pub release_cfg: bool,

        pub customizations: Customizations,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_openhcl_igvm_from_recipe::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            artifact_dir,
            done,

            base_recipe,
            release,
            release_cfg,

            customizations,
        } = request;

        let has_customizations = customizations != Customizations::default();

        let Customizations {
            build_label,
            custom_directory,
            custom_kernel_modules,
            custom_kernel,
            custom_layer,
            override_manifest,
            custom_openhcl_boot,
            custom_openvmm_hcl,
            custom_sidecar,
            custom_uefi,
            custom_vtl0_kernel,
            override_arch,
            override_kernel_pkg,
            override_openvmm_hcl_feature,
            override_max_trace_level,
            with_debuginfo,
            with_perf_tools,
            with_sidecar,
            custom_extra_rootfs,
        } = customizations;

        if release_cfg && !release {
            log::warn!(
                "You are building a debug binary with a release configuration.\n\
                The produced binary likely will not function properly due to memory restrictions."
            )
        }

        let build_profile = if release {
            OpenvmmHclBuildProfile::OpenvmmHclShip
        } else {
            OpenvmmHclBuildProfile::Debug
        };
        let mut recipe_details = base_recipe.recipe_details(release_cfg);

        {
            let OpenhclIgvmRecipeDetails {
                local_only,
                igvm_manifest,
                openhcl_kernel_package,
                openvmm_hcl_features,
                target,
                vtl0_kernel_type,
                with_uefi,
                with_interactive,
                with_sidecar: with_sidecar_details,
                max_trace_level,
            } = &mut recipe_details;

            if custom_kernel.is_some() {
                *with_uefi = true
            }

            if with_sidecar || custom_sidecar.is_some() {
                *with_sidecar_details = true;
            }

            // Debug configurations include --interactive by default, for busybox, gdbserver, and perf.
            *with_interactive = !release_cfg || with_perf_tools;

            assert!(local_only.is_none());
            *local_only = Some(OpenhclIgvmRecipeDetailsLocalOnly {
                // ensure binary remains un-sripped if perf tooling was also
                // requested
                openvmm_hcl_no_strip: with_perf_tools || with_debuginfo,
                openhcl_initrd_extra_params: Some(OpenhclInitrdExtraParams {
                    extra_initrd_layers: custom_layer
                        .into_iter()
                        .map(|p| p.absolute())
                        .collect::<Result<_, _>>()?,
                    extra_initrd_directories: custom_directory
                        .into_iter()
                        .map(|p| p.absolute())
                        .collect::<Result<_, _>>()?,
                    custom_kernel_modules,
                }),
                custom_openvmm_hcl: custom_openvmm_hcl.map(|p| p.absolute()).transpose()?,
                custom_openhcl_boot: custom_openhcl_boot.map(|p| p.absolute()).transpose()?,
                custom_uefi: custom_uefi.map(|p| p.absolute()).transpose()?,
                custom_kernel: custom_kernel.map(|p| p.absolute()).transpose()?,
                custom_sidecar: custom_sidecar.map(|p| p.absolute()).transpose()?,
                custom_extra_rootfs: custom_extra_rootfs
                    .into_iter()
                    .map(|p| p.absolute())
                    .collect::<Result<_, _>>()?,
            });

            if let Some(p) = override_manifest {
                *igvm_manifest = IgvmManifestPath::LocalOnlyCustom(p.absolute()?);
            }

            if let Some(override_kernel_pkg) = override_kernel_pkg {
                *openhcl_kernel_package = override_kernel_pkg;
            }

            if !override_openvmm_hcl_feature.is_empty() {
                *openvmm_hcl_features = override_openvmm_hcl_feature
                    .into_iter()
                    .map(OpenvmmHclFeature::LocalOnlyCustom)
                    .collect()
            }

            if let Some(arch) = override_arch {
                *target = match arch {
                    CommonArch::X86_64 => CommonTriple::X86_64_LINUX_MUSL,
                    CommonArch::Aarch64 => CommonTriple::AARCH64_LINUX_MUSL,
                };
            }

            if let Some(lvl) = override_max_trace_level {
                *max_trace_level = lvl;
            }

            if let Some(p) = custom_vtl0_kernel {
                *vtl0_kernel_type = Some(Vtl0KernelType::LocalOnlyCustom(p.absolute()?))
            }
        }

        let build_label = if let Some(label) = build_label {
            label
        } else {
            let base = match &recipe_details.igvm_manifest {
                IgvmManifestPath::InTree(_) => {
                    non_production_build_igvm_tool_out_name(&base_recipe).to_string()
                }
                IgvmManifestPath::LocalOnlyCustom(path) => path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .strip_suffix(".json")
                    .unwrap()
                    .to_string(),
            };

            if has_customizations {
                format!("{base}-custom")
            } else {
                base
            }
        };

        let (built_openvmm_hcl, write_built_openvmm_hcl) = ctx.new_var();
        let (built_openhcl_boot, write_built_openhcl_boot) = ctx.new_var();
        let (built_openhcl_igvm, write_built_openhcl_igvm) = ctx.new_var();
        let (built_sidecar, write_built_sidecar) = ctx.new_var();

        ctx.req(crate::build_openhcl_igvm_from_recipe::Request {
            build_profile,
            release_cfg,
            recipe: OpenhclIgvmRecipe::LocalOnlyCustom(recipe_details),
            custom_target: None,
            built_openvmm_hcl: write_built_openvmm_hcl,
            built_openhcl_boot: write_built_openhcl_boot,
            built_openhcl_igvm: write_built_openhcl_igvm,
            built_sidecar: write_built_sidecar,
        });

        ctx.emit_rust_step("copy to output directory", |ctx| {
            done.claim(ctx);
            let artifact_dir = artifact_dir.claim(ctx);
            let built_openvmm_hcl = built_openvmm_hcl.claim(ctx);
            let built_openhcl_boot = built_openhcl_boot.claim(ctx);
            let built_openhcl_igvm = built_openhcl_igvm.claim(ctx);
            let built_sidecar = built_sidecar.claim(ctx);
            move |rt| {
                let output_dir = rt
                    .read(artifact_dir)
                    .join(match build_profile {
                        OpenvmmHclBuildProfile::Debug => "debug",
                        OpenvmmHclBuildProfile::Release => "release",
                        OpenvmmHclBuildProfile::OpenvmmHclShip => "ship",
                    })
                    .join(&build_label);
                fs_err::create_dir_all(&output_dir)?;

                let OpenvmmHclOutput { bin, dbg } = rt.read(built_openvmm_hcl);
                fs_err::copy(bin, output_dir.join("openvmm_hcl"))?;
                if let Some(dbg) = dbg {
                    fs_err::copy(dbg, output_dir.join("openvmm_hcl.dbg"))?;
                }

                let OpenhclBootOutput { bin, dbg } = rt.read(built_openhcl_boot);
                fs_err::copy(bin, output_dir.join("openhcl_boot"))?;
                fs_err::copy(dbg, output_dir.join("openhcl_boot.dbg"))?;

                if let Some(built_sidecar) = rt.read(built_sidecar) {
                    let crate::build_sidecar::SidecarOutput { bin, dbg } = built_sidecar;
                    fs_err::copy(bin, output_dir.join("sidecar"))?;
                    fs_err::copy(dbg, output_dir.join("sidecar.dbg"))?;
                }

                let IgvmOutput {
                    igvm_bin,
                    igvm_map,
                    igvm_tdx_json,
                    igvm_snp_json,
                    igvm_vbs_json,
                } = rt.read(built_openhcl_igvm);
                fs_err::copy(
                    igvm_bin,
                    output_dir.join(format!("openhcl-{build_label}.bin")),
                )?;
                if let Some(igvm_map) = igvm_map {
                    fs_err::copy(
                        igvm_map,
                        output_dir.join(format!("openhcl-{build_label}.bin.map")),
                    )?;
                }
                if let Some(igvm_tdx_json) = igvm_tdx_json {
                    fs_err::copy(igvm_tdx_json, output_dir.join("openhcl-tdx.json"))?;
                }
                if let Some(igvm_snp_json) = igvm_snp_json {
                    fs_err::copy(igvm_snp_json, output_dir.join("openhcl-snp.json"))?;
                }
                if let Some(igvm_vbs_json) = igvm_vbs_json {
                    fs_err::copy(igvm_vbs_json, output_dir.join("openhcl-vbs.json"))?;
                }
                for e in fs_err::read_dir(output_dir)? {
                    let e = e?;
                    log::info!("{}", e.path().display());
                }

                Ok(())
            }
        });

        Ok(())
    }
}

pub fn non_production_build_igvm_tool_out_name(recipe: &OpenhclIgvmRecipe) -> &'static str {
    match recipe {
        OpenhclIgvmRecipe::X64 => "x64",
        OpenhclIgvmRecipe::X64Devkern => "x64-devkern",
        OpenhclIgvmRecipe::X64TestLinuxDirect => "x64-test-linux-direct",
        OpenhclIgvmRecipe::X64TestLinuxDirectDevkern => "x64-test-linux-direct-devkern",
        OpenhclIgvmRecipe::X64Cvm => "x64-cvm",
        OpenhclIgvmRecipe::X64CvmDevkern => "x64-cvm-devkern",
        OpenhclIgvmRecipe::Aarch64 => "aarch64",
        OpenhclIgvmRecipe::Aarch64Devkern => "aarch64-devkern",
        OpenhclIgvmRecipe::LocalOnlyCustom(_) => unreachable!(),
    }
}
