// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `hypestv` binaries

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct HypestvOutput {
    #[serde(rename = "hypestv.exe")]
    pub exe: PathBuf,
    #[serde(rename = "hypestv.pdb")]
    pub pdb: PathBuf,
}

impl Artifact for HypestvOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub hypestv: WriteVar<HypestvOutput>,
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
            hypestv,
        } = request;
        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "hypestv".into(),
            out_name: "hypestv".into(),
            crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
            profile: profile.into(),
            features: Default::default(),
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps: Vec::new(),
            output: v,
        });

        ctx.emit_minor_rust_step("report built hypestv", |ctx| {
            let hypestv = hypestv.claim(ctx);
            let output = output.claim(ctx);
            move |rt| {
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                        HypestvOutput { exe, pdb }
                    }
                    _ => unreachable!(),
                };

                rt.write(hypestv, &output);
            }
        });

        Ok(())
    }
}
