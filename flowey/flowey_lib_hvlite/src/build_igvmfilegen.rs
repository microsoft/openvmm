// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `igvmfilegen` binaries

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub enum IgvmfilegenOutput {
    LinuxBin { bin: PathBuf, dbg: PathBuf },
    WindowsBin { exe: PathBuf, pdb: PathBuf },
}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct IgvmfilegenBuildParams {
    pub target: CommonTriple,
    pub profile: CommonProfile,
}

flowey_request! {
    pub struct Request {
        pub build_params: IgvmfilegenBuildParams,
        pub igvmfilegen: WriteVar<IgvmfilegenOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        // de-dupe incoming requests
        let requests = requests
            .into_iter()
            .fold(BTreeMap::<_, Vec<_>>::new(), |mut m, r| {
                let Request {
                    build_params,
                    igvmfilegen,
                } = r;
                m.entry(build_params).or_default().push(igvmfilegen);
                m
            });

        for (IgvmfilegenBuildParams { target, profile }, outvars) in requests {
            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "igvmfilegen".into(),
                out_name: "igvmfilegen".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile: profile.into(),
                features: Default::default(),
                target: target.as_triple(),
                no_split_dbg_info: false,
                extra_env: None,
                pre_build_deps: Vec::new(),
                output: v,
            });

            ctx.emit_minor_rust_step("report built igvmfilegen", |ctx| {
                let outvars = outvars.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                            IgvmfilegenOutput::WindowsBin { exe, pdb }
                        }
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            IgvmfilegenOutput::LinuxBin {
                                bin,
                                dbg: dbg.unwrap(),
                            }
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
