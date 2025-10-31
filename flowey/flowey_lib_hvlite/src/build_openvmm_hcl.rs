// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `openvmm_hcl` binaries (NOT IGVM FILES!)

use crate::init_openvmm_magicpath_openhcl_sysroot::OpenvmmSysrootArch;
use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoFeatureSet;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OpenvmmHclFeature {
    Gdb,
    Tpm,
    LocalOnlyCustom(String),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OpenvmmHclBuildProfile {
    Debug,
    Release,
    OpenvmmHclShip,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MaxTraceLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Off,
}

impl MaxTraceLevel {
    pub fn features(&self) -> Vec<String> {
        let name = match self {
            MaxTraceLevel::Trace => return Vec::new(),
            MaxTraceLevel::Debug => "debug",
            MaxTraceLevel::Info => "info",
            MaxTraceLevel::Warn => "warn",
            MaxTraceLevel::Error => "error",
            MaxTraceLevel::Off => "off",
        };
        // Add both release and non-release variants of the feature
        // regardless of the profile to work around `tracing` bugs.
        vec![
            format!("tracing/max_level_{}", name),
            format!("tracing/release_max_level_{}", name),
        ]
    }
}

#[derive(Serialize, Deserialize)]
pub struct OpenvmmHclOutput {
    pub bin: PathBuf,
    pub dbg: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct OpenvmmHclBuildParams {
    pub target: CommonTriple,
    pub profile: OpenvmmHclBuildProfile,
    pub features: BTreeSet<OpenvmmHclFeature>,
    pub no_split_dbg_info: bool,
    pub max_trace_level: MaxTraceLevel,
}

flowey_request! {
    pub struct Request {
        pub build_params: OpenvmmHclBuildParams,
        pub openvmm_hcl_output: WriteVar<OpenvmmHclOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
        ctx.import::<crate::init_openvmm_magicpath_openhcl_sysroot::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        // de-dupe incoming requests
        let requests = requests
            .into_iter()
            .fold(BTreeMap::<_, Vec<_>>::new(), |mut m, r| {
                let Request {
                    build_params,
                    openvmm_hcl_output,
                } = r;
                m.entry(build_params).or_default().push(openvmm_hcl_output);
                m
            });

        // -- end of req processing -- //

        for (
            OpenvmmHclBuildParams {
                target,
                profile,
                features,
                no_split_dbg_info,
                max_trace_level,
            },
            outvars,
        ) in requests
        {
            let mut pre_build_deps = Vec::new();

            let target = target.as_triple();

            let arch = CommonArch::from_triple(&target).ok_or_else(|| {
                anyhow::anyhow!("cannot build openvmm_hcl on {}", target.architecture)
            })?;

            let openhcl_deps_path =
                ctx.reqv(|v| crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                    arch: match arch {
                        CommonArch::X86_64 => OpenvmmSysrootArch::X64,
                        CommonArch::Aarch64 => OpenvmmSysrootArch::Aarch64,
                    },
                    path: v,
                });

            // required due to ambient dependencies in openvmm_hcl's source code
            pre_build_deps.push(openhcl_deps_path.clone().into_side_effect());

            let mut features = features
                .into_iter()
                .map(|f| match f {
                    OpenvmmHclFeature::Gdb => "gdb".into(),
                    OpenvmmHclFeature::Tpm => "tpm".into(),
                    OpenvmmHclFeature::LocalOnlyCustom(s) => s,
                })
                .collect::<Vec<String>>();

            features.extend(max_trace_level.features());

            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "openvmm_hcl".into(),
                out_name: "openvmm_hcl".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile: match profile {
                    OpenvmmHclBuildProfile::Debug => crate::run_cargo_build::BuildProfile::Debug,
                    OpenvmmHclBuildProfile::Release => {
                        crate::run_cargo_build::BuildProfile::Release
                    }
                    OpenvmmHclBuildProfile::OpenvmmHclShip => {
                        crate::run_cargo_build::BuildProfile::UnderhillShip
                    }
                },
                features: CargoFeatureSet::Specific(features),
                target,
                no_split_dbg_info,
                extra_env: None,
                pre_build_deps,
                output: v,
            });

            ctx.emit_minor_rust_step("report built openvmm_hcl", |ctx| {
                let outvars = outvars.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            OpenvmmHclOutput { bin, dbg }
                        }
                        _ => unreachable!(),
                    };

                    for var in outvars {
                        rt.write(var, &output);
                    }
                }
            });
        }

        Ok(())
    }
}
