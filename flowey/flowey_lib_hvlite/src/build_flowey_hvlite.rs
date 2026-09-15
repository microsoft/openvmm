// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Flowey building itself

use crate::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum FloweyHvliteOutput {
    LinuxBin {
        #[serde(rename = "flowey_hvlite")]
        bin: PathBuf,
        #[serde(rename = "flowey_hvlite.dbg")]
        dbg: Option<PathBuf>,
    },
    WindowsBin {
        #[serde(rename = "flowey_hvlite.exe")]
        exe: PathBuf,
        #[serde(rename = "flowey_hvlite.pdb")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pdb: Option<PathBuf>,
    },
}

impl Artifact for FloweyHvliteOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub flowey_hvlite: WriteVar<FloweyHvliteOutput>,
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
            flowey_hvlite,
        } = request;

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "flowey_hvlite".into(),
            out_name: "flowey_hvlite".into(),
            profile: crate::run_cargo_build::BuildProfile::Light,
            features: Default::default(),
            crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps: Vec::new(),
            output: v,
        });

        ctx.emit_minor_rust_step("report built flowey_hvlite", |ctx| {
            let flowey_hvlite = flowey_hvlite.claim(ctx);
            let output = output.claim(ctx);
            move |rt| {
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                        FloweyHvliteOutput::WindowsBin { exe, pdb }
                    }
                    crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                        FloweyHvliteOutput::LinuxBin { bin, dbg }
                    }
                    _ => unreachable!(),
                };

                rt.write(flowey_hvlite, &output);
            }
        });

        Ok(())
    }
}
