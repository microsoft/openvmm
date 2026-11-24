// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `openhcl_boot` binaries

use crate::common::CommonArch;
use crate::run_cargo_build::BuildProfile;
use flowey::node::prelude::*;
use flowey_lib_common::_util::group_by;
use flowey_lib_common::run_cargo_build::CargoFeatureSet;

#[derive(Serialize, Deserialize)]
pub struct OpenhclBootOutput {
    #[serde(rename = "openhcl_boot")]
    pub bin: PathBuf,
    #[serde(rename = "openhcl_boot.dbg")]
    pub dbg: PathBuf,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OpenhclBootBuildProfile {
    Debug,
    Release,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct OpenhclBootBuildParams {
    pub arch: CommonArch,
    pub profile: OpenhclBootBuildProfile,
}

flowey_request! {
    pub struct Request {
        pub build_params: OpenhclBootBuildParams,
        pub openhcl_boot: WriteVar<OpenhclBootOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let requests = group_by(
            requests
                .into_iter()
                .map(|r| (r.build_params, r.openhcl_boot)),
        );

        for (OpenhclBootBuildParams { arch, profile }, openhcl_boot) in requests {
            let target = arch.minimal_rt_triple();

            // We use special profiles for boot, convert from the standard ones:
            let profile = match profile {
                OpenhclBootBuildProfile::Debug => BuildProfile::BootDev,
                OpenhclBootBuildProfile::Release => BuildProfile::BootRelease,
            };

            // Enable cvm_boot_log in debug builds to include TDX/SNP
            // serial logging support.
            let features = if matches!(profile, BuildProfile::BootDev) {
                CargoFeatureSet::Specific(vec!["cvm_boot_log".into()])
            } else {
                CargoFeatureSet::None
            };

            let mut extra_env = crate::common::openhcl_build_env();
            extra_env.insert("RUSTC_BOOTSTRAP".into(), "1".into());

            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "openhcl_boot".into(),
                out_name: "openhcl_boot".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile,
                features,
                target,
                no_split_dbg_info: false,
                extra_env: Some(ReadVar::from_static(extra_env)),
                pre_build_deps: Vec::new(),
                output: v,
            });

            ctx.emit_minor_rust_step("report built openhcl_boot", |ctx| {
                let openhcl_boot = openhcl_boot.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            OpenhclBootOutput {
                                bin,
                                dbg: dbg.unwrap(),
                            }
                        }
                        _ => unreachable!(),
                    };

                    rt.write_all(openhcl_boot, &output);
                }
            });
        }

        Ok(())
    }
}
