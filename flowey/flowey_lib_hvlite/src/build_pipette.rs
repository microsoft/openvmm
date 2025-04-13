// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `pipette` binaries

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum PipetteOutput {
    LinuxBin {
        #[serde(rename = "pipette")]
        bin: PathBuf,
        #[serde(rename = "pipette.dbg")]
        dbg: PathBuf,
    },
    WindowsBin {
        #[serde(rename = "pipette.exe")]
        exe: PathBuf,
        #[serde(rename = "pipette.pdb")]
        pdb: PathBuf,
    },
}

impl Artifact for PipetteOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub pipette: WriteVar<PipetteOutput>,
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
            pipette,
        } = request;

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "pipette".into(),
            out_name: "pipette".into(),
            crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
            profile: profile.into(),
            features: Default::default(),
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps: Vec::new(),
            output: v,
        });

        ctx.emit_minor_rust_step("report built pipette", |ctx| {
            let pipette = pipette.claim(ctx);
            let output = output.claim(ctx);
            move |rt| {
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                        PipetteOutput::WindowsBin { exe, pdb }
                    }
                    crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                        PipetteOutput::LinuxBin {
                            bin,
                            dbg: dbg.unwrap(),
                        }
                    }
                    _ => unreachable!(),
                };

                rt.write(pipette, &output);
            }
        });

        Ok(())
    }
}
