// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds and publishes a set of OpenHCL IGVM files, sharing identical builds
//! across requests while retaining all requested outputs.

use super::build_and_publish_openvmm_hcl_baseline;
use crate::_jobs::build_and_publish_openvmm_hcl_baseline::OpenvmmHclBaselineOutput;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmExtrasOutput;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmOutput;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipeType;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::build_openvmm_hcl::OpenvmmHclFeature;
use crate::build_vmfirmwareigvm_dll::VmfirmwareigvmDllOutput;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::_util::group_by;
use petri_artifacts_core::ArtifactId;
use std::collections::BTreeSet;

#[derive(Serialize, Deserialize)]
pub struct VmfirmwareigvmDllParams {
    pub internal_dll_name: String,
    pub dll_version: (u16, u16, u16, u16),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
pub struct OpenhclIgvmOutputs {
    pub openhcl_igvm: WriteVar<OpenhclIgvmOutput>,
    pub openhcl_igvm_extras: WriteVar<OpenhclIgvmExtrasOutput>,
    pub vmfirmwareigvm_dll: Option<WriteVar<VmfirmwareigvmDllOutput>>,
    pub artifact_openhcl_verify_size_baseline: Option<WriteVar<OpenvmmHclBaselineOutput>>,
}

flowey_request! {
    pub struct Params {
        pub igvm_files: Vec<(OpenhclIgvmBuildParams, OpenhclIgvmOutputs)>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_openvmm_hcl_sizecheck::publish::Node>();
        ctx.import::<crate::build_openhcl_igvm_from_recipe::Node>();
        ctx.import::<crate::build_vmfirmwareigvm_dll::Node>();
        ctx.import::<build_and_publish_openvmm_hcl_baseline::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let requests = group_by(requests.into_iter().flat_map(|r| r.igvm_files));

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
            outputs,
        ) in requests
        {
            let (built_igvm_extras, openhcl_igvm_extras) = ctx.new_var();
            let built_igvm = ctx.reqv(|v| crate::build_openhcl_igvm_from_recipe::Request {
                custom_target: custom_target.clone(),
                build_profile: profile,
                release_cfg,
                recipe: OpenhclIgvmRecipeType::WellKnown(recipe.clone()),
                extra_features,
                disable_secure_avic: false,
                uefi_firmware_flavor,
                confidential_debug,
                openhcl_igvm: v,
                openhcl_igvm_extras,
            });
            let mut built_dll = None;

            for OpenhclIgvmOutputs {
                openhcl_igvm,
                openhcl_igvm_extras,
                vmfirmwareigvm_dll,
                artifact_openhcl_verify_size_baseline,
            } in outputs
            {
                built_igvm.write_into(ctx, openhcl_igvm);
                built_igvm_extras.write_into(ctx, openhcl_igvm_extras);

                if let Some(vmfirmwareigvm_dll) = vmfirmwareigvm_dll {
                    let built_dll = built_dll.get_or_insert_with(|| {
                        ctx.reqv(|v| crate::build_vmfirmwareigvm_dll::Request {
                            arch: recipe.common_arch(),
                            openhcl_igvm: built_igvm.clone(),
                            resource_id: crate::build_vmfirmwareigvm_dll::SNP_RESOURCE_ID,
                            dll_version: ReadVar::from_static(
                                crate::build_vmfirmwareigvm_dll::UNUSED_DLL_VERSION,
                            ),
                            internal_dll_name:
                                petri_artifacts_vmm_test::artifacts::vmfw_dll::LATEST_CVM_X64::FILENAME
                                    .into(),
                            vmfirmwareigvm_dll: v,
                        })
                    });
                    built_dll.write_into(ctx, vmfirmwareigvm_dll);
                }

                if let Some(sizecheck_artifact) = artifact_openhcl_verify_size_baseline {
                    ctx.req(build_and_publish_openvmm_hcl_baseline::Request {
                        target: custom_target
                            .clone()
                            .unwrap_or_else(|| recipe.recipe_details(release_cfg).target),
                        baseline: sizecheck_artifact,
                    });
                }
            }
        }

        Ok(())
    }
}
