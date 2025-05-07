// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Builds and publishes an a set of OpenHCL IGVM files.

use super::build_and_publish_openvmm_hcl_baseline;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::run_cargo_build::common::CommonTriple;
use crate::run_igvmfilegen::IgvmOutput;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

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
        pub openhcl_igvm: WriteVar<OpenhclIgvmSet>,
        pub openhcl_igvm_extras: WriteVar<OpenhclIgvmExtrasSet>,
        pub artifact_openhcl_verify_size_baseline: Option<ReadVar<PathBuf>>,
    }
}

pub struct OpenhclIgvmSet(pub Vec<(OpenhclIgvmRecipe, IgvmOutput)>);

impl Artifact for OpenhclIgvmSet {}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpenhclIgvmExtrasSet(pub BTreeMap<OpenhclIgvmRecipe, OpenhclIgvmExtras>);

impl Artifact for OpenhclIgvmExtrasSet {}

#[derive(Serialize, Deserialize)]
pub struct OpenhclIgvmExtras {
    #[serde(flatten)]
    pub openvmm_hcl_bin: crate::build_openvmm_hcl::OpenvmmHclOutput,
    #[serde(rename = "openhcl.bin.map")]
    pub openhcl_map: Option<PathBuf>,
    #[serde(flatten)]
    pub openhcl_boot: crate::build_openhcl_boot::OpenhclBootOutput,
    #[serde(flatten)]
    pub sidecar: Option<crate::build_sidecar::SidecarOutput>,
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
            openhcl_igvm,
            openhcl_igvm_extras,
            artifact_openhcl_verify_size_baseline,
        } = request;

        let output = igvm_files
            .into_iter()
            .map(
                |OpenhclIgvmBuildParams {
                     profile,
                     recipe,
                     custom_target,
                 }| {
                    (
                        recipe,
                        ctx.reqv(|v| crate::build_openhcl_igvm_from_recipe::Request {
                            custom_target,
                            profile,
                            recipe: recipe.into(),
                            output: v,
                        }),
                    )
                },
            )
            .collect::<Vec<_>>();

        let sizecheck_artifact = artifact_openhcl_verify_size_baseline.map(|artifact_dir| {
            ctx.reqv(|v| build_and_publish_openvmm_hcl_baseline::Request {
                artifact_dir,
                done: v,
            })
        });

        ctx.emit_minor_rust_step("collect openhcl results", |ctx| {
            let output = output
                .into_iter()
                .map(|(r, v)| (r, v.claim(ctx)))
                .collect::<Vec<_>>();
            let openhcl_igvm = openhcl_igvm.claim(ctx);
            let openhcl_igvm_extras = openhcl_igvm_extras.claim(ctx);
            sizecheck_artifact.claim(ctx);
            move |rt| {
                let (base, extras) = output
                    .into_iter()
                    .map(|(r, v)| {
                        let v = rt.read(v);
                        let extras = OpenhclIgvmExtras {
                            openvmm_hcl_bin: v.openvmm_hcl,
                            openhcl_map: v.igvm.igvm_map.clone(),
                            openhcl_boot: v.openhcl_boot,
                            sidecar: v.sidecar,
                        };
                        let base = v.igvm;
                        ((r, base), (r, extras))
                    })
                    .unzip();
                rt.write(openhcl_igvm, &OpenhclIgvmSet(base));
                rt.write(openhcl_igvm_extras, &OpenhclIgvmExtrasSet(extras));
            }
        });

        Ok(())
    }
}

/// Custom logic for serializing and deserializing the [`OpenhclIgvmSet`]` with a
/// directory structure we like.
mod artifact {
    use super::OpenhclIgvmSet;
    use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
    use crate::run_igvmfilegen::IgvmOutput;
    use serde::Deserialize;
    use serde::Serialize;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    impl Serialize for OpenhclIgvmSet {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let mut v = BTreeMap::new();
            for (recipe, value) in &self.0 {
                let IgvmOutput {
                    igvm_bin,
                    igvm_map,
                    igvm_tdx_json,
                    igvm_snp_json,
                    igvm_vbs_json,
                } = value;
                let Value::String(recipe) = serde_json::to_value(recipe).unwrap() else {
                    unreachable!()
                };
                v.insert(format!("{}.bin", recipe), igvm_bin);
                if let Some(igvm_map) = igvm_map {
                    v.insert(format!("{}.bin.map", recipe), igvm_map);
                }
                if let Some(igvm_tdx_json) = igvm_tdx_json {
                    v.insert(format!("{}-tdx.json", recipe), igvm_tdx_json);
                }
                if let Some(igvm_snp_json) = igvm_snp_json {
                    v.insert(format!("{}-snp.json", recipe), igvm_snp_json);
                }
                if let Some(igvm_vbs_json) = igvm_vbs_json {
                    v.insert(format!("{}-vbs.json", recipe), igvm_vbs_json);
                }
            }
            v.serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for OpenhclIgvmSet {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let map = BTreeMap::<String, PathBuf>::deserialize(deserializer)?;
            let mut r = Vec::new();
            for (name, igvm_bin) in &map {
                let Some(v) = name.strip_suffix(".bin") else {
                    continue;
                };
                let recipe: OpenhclIgvmRecipe =
                    serde_json::from_value(Value::String(v.to_string()))
                        .map_err(serde::de::Error::custom)?;
                let igvm_map = map.get(&format!("{}.bin.map", v)).cloned();
                let igvm_tdx_json = map.get(&format!("{}-tdx.json", v)).cloned();
                let igvm_snp_json = map.get(&format!("{}-snp.json", v)).cloned();
                let igvm_vbs_json = map.get(&format!("{}-vbs.json", v)).cloned();
                let igvm = IgvmOutput {
                    igvm_bin: igvm_bin.clone(),
                    igvm_map,
                    igvm_tdx_json,
                    igvm_snp_json,
                    igvm_vbs_json,
                };
                r.push((recipe, igvm));
            }
            Ok(OpenhclIgvmSet(r))
        }
    }
}
