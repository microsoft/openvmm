// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `prep_steps` binaries

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum PrepStepsOutput {
    LinuxBin {
        #[serde(rename = "prep_steps")]
        bin: PathBuf,
        #[serde(rename = "prep_steps.dbg")]
        dbg: PathBuf,
    },
    WindowsBin {
        #[serde(rename = "prep_steps.exe")]
        exe: PathBuf,
        #[serde(rename = "prep_steps.pdb")]
        pdb: PathBuf,
    },
}

impl Artifact for PrepStepsOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub prep_steps: WriteVar<PrepStepsOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            profile,
            prep_steps,
        } = request;

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "prep_steps".into(),
            out_name: "prep_steps".into(),
            crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
            profile: profile.into(),
            features: Default::default(),
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps: Vec::new(),
            output: v,
        });

        ctx.emit_minor_rust_step("report built prep_steps", |ctx| {
            let prep_steps = prep_steps.claim(ctx);
            let output = output.claim(ctx);
            move |rt| {
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                        PrepStepsOutput::WindowsBin { exe, pdb }
                    }
                    crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                        PrepStepsOutput::LinuxBin {
                            bin,
                            dbg: dbg.unwrap(),
                        }
                    }
                    _ => unreachable!(),
                };

                rt.write(prep_steps, &output);
            }
        });

        Ok(())
    }
}
