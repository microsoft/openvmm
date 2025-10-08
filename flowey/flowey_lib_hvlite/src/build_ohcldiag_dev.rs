// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `ohcldiag_dev` binaries

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum OhcldiagDevOutput {
    LinuxBin {
        #[serde(rename = "ohcldiag-dev")]
        bin: PathBuf,
        #[serde(rename = "ohcldiag-dev.dbg")]
        dbg: PathBuf,
    },
    WindowsBin {
        #[serde(rename = "ohcldiag-dev.exe")]
        exe: PathBuf,
        #[serde(rename = "ohcldiag-dev.pdb")]
        pdb: PathBuf,
    },
}

impl Artifact for OhcldiagDevOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub ohcldiag_dev: WriteVar<OhcldiagDevOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            profile,
            ohcldiag_dev,
        } = request;
        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "ohcldiag-dev".into(),
            out_name: "ohcldiag-dev".into(),
            crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
            profile: profile.into(),
            features: Default::default(),
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps: Vec::new(),
            output: v,
        });

        ctx.emit_minor_rust_step("report built ohcldiag_dev", |ctx| {
            let ohcldiag_dev = ohcldiag_dev.claim(ctx);
            let output = output.claim(ctx);
            move |rt| {
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                        OhcldiagDevOutput::WindowsBin { exe, pdb }
                    }
                    crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                        OhcldiagDevOutput::LinuxBin {
                            bin,
                            dbg: dbg.unwrap(),
                        }
                    }
                    _ => unreachable!(),
                };

                rt.write(ohcldiag_dev, &output);
            }
        });

        Ok(())
    }
}
