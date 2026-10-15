// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `pipette` binaries

use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::_util::group_by;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum PipetteOutput {
    LinuxBin {
        #[serde(rename = "pipette")]
        bin: PathBuf,
        #[serde(rename = "pipette.dbg")]
        dbg: Option<PathBuf>,
    },
    WindowsBin {
        #[serde(rename = "pipette.exe")]
        exe: PathBuf,
        #[serde(rename = "pipette.pdb")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pdb: Option<PathBuf>,
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
                .map(|r| ((r.target, r.profile), r.pipette)),
        );
        for ((target, profile), pipette) in requests {
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
                            PipetteOutput::LinuxBin { bin, dbg }
                        }
                        _ => unreachable!(),
                    };

                    rt.write_all(pipette, &output);
                }
            });
        }

        Ok(())
    }
}
