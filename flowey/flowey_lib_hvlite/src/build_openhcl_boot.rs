// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `openhcl_boot` binaries

use crate::run_cargo_build::BuildProfile;
use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct OpenhclBootOutput {
    pub bin: PathBuf,
    pub dbg: PathBuf,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OpenhclBootBuildProfile {
    Debug,
    Release,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct OpenhclBootBuildParams {
    pub arch: CommonArch,
    pub profile: OpenhclBootBuildProfile,
}

flowey_request! {
    pub struct Request {
        pub build_params: OpenhclBootBuildParams,
        pub openhcl_boot: WriteVar<OpenhclBootOutput>,
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
                    build_params,
                    openhcl_boot,
                } = r;
                m.entry(build_params).or_default().push(openhcl_boot);
                m
            });

        for (OpenhclBootBuildParams { arch, profile }, openhcl_boot) in requests {
            let target = target_lexicon::Triple {
                architecture: arch.as_arch(),
                operating_system: target_lexicon::OperatingSystem::None_,
                environment: target_lexicon::Environment::Unknown,
                vendor: target_lexicon::Vendor::Custom(target_lexicon::CustomVendor::Static(
                    "minimal_rt",
                )),
                binary_format: target_lexicon::BinaryFormat::Unknown,
            };

            // We use special profiles for boot, convert from the standard ones:
            let profile = match profile {
                OpenhclBootBuildProfile::Debug => BuildProfile::BootDev,
                OpenhclBootBuildProfile::Release => BuildProfile::BootRelease,
            };

            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "openhcl_boot".into(),
                out_name: "openhcl_boot".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile,
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

            ctx.emit_minor_rust_step("report built openhcl_boot", |ctx| {
                let openhcl_boot = openhcl_boot.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            OpenhclBootOutput {
                                bin,
                                dbg: dbg.unwrap(),
                            }
                        }
                        _ => unreachable!(),
                    };

                    for var in openhcl_boot {
                        rt.write(var, &output);
                    }
                }
            });
        }

        Ok(())
    }
}
