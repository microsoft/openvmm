// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the standalone VMM.Perf runner.

use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum VmmPerfOutput {
    WindowsBin {
        #[serde(rename = "vmm_perf.exe")]
        exe: PathBuf,
        #[serde(rename = "vmm_perf.pdb")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pdb: Option<PathBuf>,
    },
    LinuxBin {
        #[serde(rename = "vmm_perf")]
        bin: PathBuf,
        #[serde(rename = "vmm_perf.dbg")]
        dbg: PathBuf,
    },
}

impl Artifact for VmmPerfOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub vmm_perf: WriteVar<VmmPerfOutput>,
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
            target,
            profile,
            vmm_perf,
        } in requests
        {
            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "vmm_perf".into(),
                out_name: "vmm_perf".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile: profile.into(),
                features: Default::default(),
                target: target.as_triple(),
                no_split_dbg_info: false,
                extra_env: None,
                pre_build_deps: Vec::new(),
                output: v,
            });

            ctx.emit_minor_rust_step("report built VMM.Perf runner", |ctx| {
                let output = output.claim(ctx);
                let vmm_perf = vmm_perf.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                            VmmPerfOutput::WindowsBin { exe, pdb }
                        }
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            VmmPerfOutput::LinuxBin {
                                bin,
                                dbg: dbg.unwrap(),
                            }
                        }
                        _ => unreachable!(),
                    };
                    rt.write(vmm_perf, &output);
                }
            });
        }

        Ok(())
    }
}
