// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds and publishes an a set of OpenHCL IGVM files.

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
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_openhcl_igvm_from_recipe_extras::publish::Node>();
        ctx.import::<crate::artifact_openhcl_igvm_from_recipe::publish::Node>();
        ctx.import::<crate::build_openhcl_igvm_from_recipe::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            igvm_files,
            artifact_dir_openhcl_igvm,
            artifact_dir_openhcl_igvm_extras,
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
            let (read_built_sidecar, built_sidecar) = ctx.new_var();
            ctx.req(crate::build_openhcl_igvm_from_recipe::Request {
                custom_target,
                profile,
                recipe: recipe.clone(),
                built_openvmm_hcl,
                built_openhcl_boot,
                built_openhcl_igvm,
                built_sidecar,
            });

            built_igvm_files.push(read_built_openhcl_igvm.map(ctx, {
                let recipe = recipe.clone();
                move |x| (recipe, x)
            }));

            built_extras.push(ctx.emit_minor_rust_stepv(
                "collect openhcl component paths",
                |ctx| {
                    let recipe = recipe.clone();
                    let read_built_openvmm_hcl = read_built_openvmm_hcl.claim(ctx);
                    let read_built_openhcl_boot = read_built_openhcl_boot.claim(ctx);
                    let read_built_openhcl_igvm = read_built_openhcl_igvm.claim(ctx);
                    let read_built_sidecar = read_built_sidecar.claim(ctx);
                    |rt| OpenhclIgvmExtras {
                        recipe,
                        openvmm_hcl_bin: rt.read(read_built_openvmm_hcl),
                        openhcl_map: rt.read(read_built_openhcl_igvm).igvm_map,
                        openhcl_boot: rt.read(read_built_openhcl_boot),
                        sidecar: rt.read(read_built_sidecar),
                    }
                },
            ));
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

        ctx.emit_side_effect_step(did_publish, [done]);

        Ok(())
    }
}
