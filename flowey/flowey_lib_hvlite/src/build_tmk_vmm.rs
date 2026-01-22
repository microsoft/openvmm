// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build TMK binaries

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum TmkVmmOutput {
    WindowsBin {
        #[serde(rename = "tmk_vmm.exe")]
        exe: PathBuf,
        #[serde(rename = "tmk_vmm.pdb")]
        pdb: PathBuf,
    },
    LinuxBin {
        #[serde(rename = "tmk_vmm")]
        bin: PathBuf,
        #[serde(rename = "tmk_vmm.dbg")]
        dbg: PathBuf,
    },
}

impl Artifact for TmkVmmOutput {}

flowey_request! {
    pub struct Request {
        pub profile: CommonProfile,
        pub target: CommonTriple,
        pub unstable_whp: bool,
        pub tmk_vmm: WriteVar<TmkVmmOutput>,
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
                    target,
                    profile,
                    unstable_whp,
                    tmk_vmm,
                } = r;
                m.entry((target, profile, unstable_whp))
                    .or_default()
                    .push(tmk_vmm);
                m
            });

        for ((target, profile, unstable_whp), tmk_vmm) in requests {
            let features = if unstable_whp && target == CommonTriple::AARCH64_WINDOWS_MSVC {
                ["unstable_whp".to_owned()].into()
            } else {
                Default::default()
            };

            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "tmk_vmm".into(),
                out_name: "tmk_vmm".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile: profile.into(),
                features,
                target: target.as_triple(),
                no_split_dbg_info: false,
                extra_env: None,
                pre_build_deps: Vec::new(),
                output: v,
            });

            ctx.emit_minor_rust_step("report built tmk_vmm", |ctx| {
                let tmk_vmm = tmk_vmm.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                            TmkVmmOutput::WindowsBin { exe, pdb }
                        }
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            TmkVmmOutput::LinuxBin {
                                bin,
                                dbg: dbg.unwrap(),
                            }
                        }
                        _ => unreachable!(),
                    };

                    for var in tmk_vmm {
                        rt.write(var, &output);
                    }
                }
            });
        }

        Ok(())
    }
}
