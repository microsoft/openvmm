// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds and publishes an a set of OpenHCL IGVM files.

use super::build_and_publish_openvmm_hcl_baseline;
use crate::artifact_openhcl_igvm_from_recipe_extras::OpenhclIgvmExtras;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct VmfirmwareigvmDllParams {
    pub internal_dll_name: String,
    pub dll_version: (u16, u16, u16, u16),
}

#[derive(Serialize, Deserialize)]
pub struct OpenhclIgvmBuildParams {
    pub profile: OpenvmmHclBuildProfile,
    pub recipe: OpenhclIgvmRecipe,
    pub custom_target: Option<CommonTriple>,
}

flowey_request! {
    pub struct Params {
        pub igvm_files: Vec<OpenhclIgvmBuildParams>,
        pub artifact_dir_openhcl_igvm: ReadVar<PathBuf>,
        pub artifact_dir_openhcl_igvm_extras: ReadVar<PathBuf>,
        pub artifact_openhcl_verify_size_baseline: Option<ReadVar<PathBuf>>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_openhcl_igvm_from_recipe_extras::publish::Node>();
        ctx.import::<crate::artifact_openhcl_igvm_from_recipe::publish::Node>();
        ctx.import::<crate::artifact_openvmm_hcl_sizecheck::publish::Node>();
        ctx.import::<crate::build_openhcl_igvm_from_recipe::Node>();
        ctx.import::<build_and_publish_openvmm_hcl_baseline::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            igvm_files,
            artifact_dir_openhcl_igvm,
            artifact_dir_openhcl_igvm_extras,
            artifact_openhcl_verify_size_baseline,
            done,
        } = request;

        let mut built_igvm_files = Vec::new();
        let mut built_extras = Vec::new();

        for OpenhclIgvmBuildParams {
            profile,
            recipe,
            custom_target,
        } in igvm_files
        {
            let (read_built_openvmm_hcl, built_openvmm_hcl) = ctx.new_var();
            let (read_built_openhcl_boot, built_openhcl_boot) = ctx.new_var();
            let (read_built_openhcl_igvm, built_openhcl_igvm) = ctx.new_var();
            ctx.req(crate::build_openhcl_igvm_from_recipe::Request {
                custom_target,
                profile,
                recipe: recipe.clone(),
                built_openvmm_hcl,
                built_openhcl_boot,
                built_openhcl_igvm,
                built_sidecar: None,
            });

            built_igvm_files.push(read_built_openhcl_igvm.map(ctx, {
                let recipe = recipe.clone();
                move |x| (recipe, x)
            }));

            built_extras.push(
                read_built_openvmm_hcl
                    .zip(ctx, read_built_openhcl_boot)
                    .zip(ctx, read_built_openhcl_igvm.clone())
                    .map(ctx, {
                        let recipe = recipe.clone();
                        |((openvmm_hcl_bin, openhcl_boot), openhcl_igvm)| OpenhclIgvmExtras {
                            recipe,
                            openvmm_hcl_bin,
                            openhcl_map: openhcl_igvm.igvm_map,
                            openhcl_boot,
                            sidecar: None,
                        }
                    }),
            );
        }

        let mut did_publish = Vec::new();

        did_publish.push(ctx.reqv(|done| {
            crate::artifact_openhcl_igvm_from_recipe::publish::Request {
                openhcl_igvm_files: built_igvm_files,
                artifact_dir: artifact_dir_openhcl_igvm,
                done,
            }
        }));

        did_publish.push(ctx.reqv(|v| {
            crate::artifact_openhcl_igvm_from_recipe_extras::publish::Request {
                extras: built_extras,
                artifact_dir: artifact_dir_openhcl_igvm_extras,
                done: v,
            }
        }));

        if let Some(sizecheck_artifact) = artifact_openhcl_verify_size_baseline {
            did_publish.push(
                ctx.reqv(|v| build_and_publish_openvmm_hcl_baseline::Request {
                    artifact_dir: sizecheck_artifact,
                    done: v,
                }),
            );
        }

        ctx.emit_side_effect_step(did_publish, [done]);

        Ok(())
    }
}
