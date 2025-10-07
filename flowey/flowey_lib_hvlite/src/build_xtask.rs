// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `xtask` binary

use crate::run_cargo_build::BuildProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoCrateType;

#[derive(Serialize, Deserialize)]
pub enum XtaskOutput {
    LinuxBin { bin: PathBuf, dbg: PathBuf },
    WindowsBin { exe: PathBuf, pdb: PathBuf },
}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub xtask: WriteVar<XtaskOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { target, xtask } = request;

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "xtask".into(),
            out_name: "xtask".into(),
            crate_type: CargoCrateType::Bin,
            profile: BuildProfile::Light,
            features: Default::default(),
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps: Vec::new(),
            output: v,
        });

        ctx.emit_minor_rust_step("report built xtask", |ctx| {
            let xtask = xtask.claim(ctx);
            let output = output.claim(ctx);
            move |rt| {
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                        XtaskOutput::WindowsBin { exe, pdb }
                    }
                    crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                        XtaskOutput::LinuxBin {
                            bin,
                            dbg: dbg.unwrap(),
                        }
                    }
                    _ => unreachable!(),
                };

                rt.write(xtask, &output);
            }
        });

        Ok(())
    }
}
