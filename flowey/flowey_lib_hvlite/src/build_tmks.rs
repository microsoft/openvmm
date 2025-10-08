// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build TMK binaries

use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonProfile;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct TmksOutput {
    #[serde(rename = "simple_tmk")]
    pub bin: PathBuf,
    #[serde(rename = "simple_tmk.dbg")]
    pub dbg: PathBuf,
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
        // de-dupe incoming requests
        let requests = requests
            .into_iter()
            .fold(BTreeMap::<_, Vec<_>>::new(), |mut m, r| {
                let Request {
                    arch,
                    profile,
                    tmks,
                } = r;
                m.entry((arch, profile)).or_default().push(tmks);
                m
            });

        for ((arch, profile), tmks) in requests {
            let target = target_lexicon::Triple {
                architecture: arch.as_arch(),
                operating_system: target_lexicon::OperatingSystem::None_,
                environment: target_lexicon::Environment::Unknown,
                vendor: target_lexicon::Vendor::Custom(target_lexicon::CustomVendor::Static(
                    "minimal_rt",
                )),
                binary_format: target_lexicon::BinaryFormat::Unknown,
            };

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
                            TmksOutput {
                                bin,
                                dbg: dbg.unwrap(),
                            }
                        }
                        _ => unreachable!(),
                    };

                    for var in tmks {
                        rt.write(var, &output);
                    }
                }
            });
        }

        Ok(())
    }
}
