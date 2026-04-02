// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the `openvmm_vhost` binary (vhost-user backend).

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct OpenvmmVhostBuildParams {
    pub profile: CommonProfile,
    pub target: CommonTriple,
}

#[derive(Serialize, Deserialize)]
pub struct OpenvmmVhostOutput {
    pub bin: PathBuf,
    pub dbg: PathBuf,
}

impl Artifact for OpenvmmVhostOutput {}

flowey_request! {
    pub struct Request {
        pub params: OpenvmmVhostBuildParams,
        pub openvmm_vhost: WriteVar<OpenvmmVhostOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        for Request {
            params: OpenvmmVhostBuildParams { profile, target },
            openvmm_vhost: openvmm_vhost_bin,
        } in requests
        {
            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "openvmm_vhost".into(),
                out_name: "openvmm_vhost".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile: profile.into(),
                features: flowey_lib_common::run_cargo_build::CargoFeatureSet::default(),
                target: target.as_triple(),
                no_split_dbg_info: false,
                extra_env: None,
                pre_build_deps: Vec::new(),
                output: v,
            });

            ctx.emit_minor_rust_step("report built openvmm_vhost", |ctx| {
                let openvmm_vhost_bin = openvmm_vhost_bin.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            OpenvmmVhostOutput {
                                bin,
                                dbg: dbg.unwrap(),
                            }
                        }
                        _ => unreachable!("openvmm_vhost is Linux-only"),
                    };

                    rt.write(openvmm_vhost_bin, &output);
                }
            });
        }

        Ok(())
    }
}
