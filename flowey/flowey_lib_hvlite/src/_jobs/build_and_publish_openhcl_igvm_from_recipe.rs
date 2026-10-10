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
use crate::build_vmfirmwareigvm_dll::VmfirmwareigvmDllOutput;
use crate::common::CommonArch;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use petri_artifacts_core::ArtifactId;
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
    pub uefi_firmware_flavor: Option<crate::download_uefi_mu_msvm::FirmwareFlavor>,
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
        /// Package the x64 CVM IGVM into a resource DLL.
        pub vmfirmwareigvm_cvm: Option<WriteVar<VmfirmwareigvmDllOutput>>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_openvmm_hcl_sizecheck::publish::Node>();
        ctx.import::<crate::build_openhcl_igvm_from_recipe::Node>();
        ctx.import::<crate::build_vmfirmwareigvm_dll::Node>();
        ctx.import::<build_and_publish_openvmm_hcl_baseline::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            igvm_files,
            artifact_openhcl_verify_size_baseline,
            mut vmfirmwareigvm_cvm,
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
                uefi_firmware_flavor,
                release_cfg,
                confidential_debug,
            },
            openhcl_igvm,
            openhcl_igvm_extras,
        ) in igvm_files
        {
            let built_igvm = ctx.reqv(|v| crate::build_openhcl_igvm_from_recipe::Request {
                custom_target: custom_target.clone(),
                build_profile: profile,
                release_cfg,
                recipe: OpenhclIgvmRecipeType::WellKnown(recipe.clone()),
                extra_features: extra_features.clone(),
                disable_secure_avic: false,
                uefi_firmware_flavor,
                confidential_debug,
                openhcl_igvm: v,
                openhcl_igvm_extras,
            });
            built_igvm.clone().write_into(ctx, openhcl_igvm);

            if recipe == OpenhclIgvmRecipe::X64Cvm {
                if let Some(vmfirmwareigvm_dll) = vmfirmwareigvm_cvm.take() {
                    ctx.req(crate::build_vmfirmwareigvm_dll::Request {
                        arch: CommonArch::X86_64,
                        openhcl_igvm: built_igvm,
                        resource_id: crate::build_vmfirmwareigvm_dll::SNP_RESOURCE_ID,
                        dll_version: ReadVar::from_static(
                            crate::build_vmfirmwareigvm_dll::UNUSED_DLL_VERSION,
                        ),
                        internal_dll_name:
                            petri_artifacts_vmm_test::artifacts::vmfw_dll::LATEST_CVM_X64::FILENAME
                                .into(),
                        vmfirmwareigvm_dll,
                    });
                }
            }
        }

        if vmfirmwareigvm_cvm.is_some() {
            anyhow::bail!("the CVM firmware DLL requires an x64 CVM IGVM");
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
