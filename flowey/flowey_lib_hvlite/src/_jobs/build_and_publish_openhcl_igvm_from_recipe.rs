// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds and publishes an a set of OpenHCL IGVM files.

use super::build_and_publish_openvmm_hcl_baseline;
use crate::_jobs::build_and_publish_openvmm_hcl_baseline::OpenvmmHclBaselineOutput;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmExtrasOutput;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmOutput;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipeType;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::build_openvmm_hcl::OpenvmmHclFeature;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use std::collections::BTreeSet;

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
    /// Additional features to enable on top of the recipe's defaults.
    pub extra_features: BTreeSet<OpenvmmHclFeature>,
    /// Whether to use release configuration (release manifests, no gdb, etc.).
    pub release_cfg: bool,
    /// Add the confidential debug flag to the measured OpenHCL command line,
    /// enabling confidential diagnostics on CVM builds. Used by the
    /// VMM tests so that release CVM IGVMs still emit diagnostics.
    pub confidential_debug: bool,
}

flowey_request! {
    pub struct Params {
        pub igvm_files: Vec<(OpenhclIgvmBuildParams, WriteVar<OpenhclIgvmOutput>, WriteVar<OpenhclIgvmExtrasOutput>)>,
        pub artifact_openhcl_verify_size_baseline: Option<WriteVar<OpenvmmHclBaselineOutput>>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_openvmm_hcl_sizecheck::publish::Node>();
        ctx.import::<crate::build_openhcl_igvm_from_recipe::Node>();
        ctx.import::<build_and_publish_openvmm_hcl_baseline::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            igvm_files,
            artifact_openhcl_verify_size_baseline,
        } = request;

        // Validate that all custom_target values are equal (or all None)
        // for baseline publishing below
        let (all_same, unique_target) = {
            let mut unique_target: Option<CommonTriple> = None;
            let mut all_same = true;
            for (params, _, _) in &igvm_files {
                match (&unique_target, &params.custom_target) {
                    (None, Some(t)) => unique_target = Some(t.clone()),
                    (Some(u), Some(t)) if u != t => {
                        all_same = false;
                        break;
                    }
                    _ => {}
                }
            }
            (all_same, unique_target)
        };

        for (
            OpenhclIgvmBuildParams {
                profile,
                recipe,
                custom_target,
                extra_features,
                release_cfg,
                confidential_debug,
            },
            openhcl_igvm,
            openhcl_igvm_extras,
        ) in igvm_files
        {
            ctx.req(crate::build_openhcl_igvm_from_recipe::Request {
                custom_target: custom_target.clone(),
                build_profile: profile,
                release_cfg,
                recipe: OpenhclIgvmRecipeType::WellKnown(recipe.clone()),
                extra_features: extra_features.clone(),
                disable_secure_avic: false,
                confidential_debug,
                openhcl_igvm,
                openhcl_igvm_extras,
            });
        }

        if let Some(sizecheck_artifact) = artifact_openhcl_verify_size_baseline {
            if all_same {
                if let Some(custom_target) = unique_target {
                    ctx.req(build_and_publish_openvmm_hcl_baseline::Request {
                        target: custom_target,
                        baseline: sizecheck_artifact,
                    });
                }
            } else {
                return Err(anyhow::anyhow!(
                    "All igvm_files must have the same custom_target for baseline build, but found differing targets."
                ));
            }
        }

        Ok(())
    }
}
