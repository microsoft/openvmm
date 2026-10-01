// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build TMK binaries

use crate::common::CommonArch;
use crate::common::CommonProfile;
use flowey::node::prelude::*;
use flowey_lib_common::_util::group_by;

#[derive(Serialize, Deserialize)]
pub struct TmksOutput {
    #[serde(rename = "simple_tmk")]
    pub bin: PathBuf,
    #[serde(rename = "simple_tmk.dbg")]
    pub dbg: Option<PathBuf>,
}

impl Artifact for TmksOutput {}

flowey_request! {
    pub struct Request {
        pub arch: CommonArch,
        pub profile: CommonProfile,
        pub tmks: WriteVar<TmksOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let requests = group_by(requests.into_iter().map(|r| ((r.arch, r.profile), r.tmks)));

        for ((arch, profile), tmks) in requests {
            let target = arch.minimal_rt_triple();

            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "simple_tmk".into(),
                out_name: "simple_tmk".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile: profile.into(),
                features: Default::default(),
                target,
                no_split_dbg_info: false,
                extra_env: Some(ReadVar::from_static(
                    [("RUSTC_BOOTSTRAP".to_string(), "1".to_string())]
                        .into_iter()
                        .collect(),
                )),
                pre_build_deps: Vec::new(),
                output: v,
            });

            ctx.emit_minor_rust_step("report built tmks", |ctx| {
                let tmks = tmks.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            TmksOutput { bin, dbg }
                        }
                        _ => unreachable!(),
                    };

                    rt.write_all(tmks, &output);
                }
            });
        }

        Ok(())
    }
}
