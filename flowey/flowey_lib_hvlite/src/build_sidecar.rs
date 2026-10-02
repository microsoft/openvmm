// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `sidecar` binaries

use crate::common::CommonArch;
use crate::run_cargo_build::BuildProfile;
use flowey::node::prelude::*;
use flowey_lib_common::_util::group_by;

#[derive(Serialize, Deserialize)]
pub struct SidecarOutput {
    #[serde(rename = "sidecar")]
    pub bin: PathBuf,
    #[serde(rename = "sidecar.dbg")]
    pub dbg: PathBuf,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SidecarBuildProfile {
    Debug,
    Release,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SidecarBuildParams {
    pub arch: CommonArch,
    pub profile: SidecarBuildProfile,
}

flowey_request! {
    pub struct Request {
        pub build_params: SidecarBuildParams,
        pub sidecar: WriteVar<SidecarOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let requests = group_by(requests.into_iter().map(|r| (r.build_params, r.sidecar)));

        for (SidecarBuildParams { arch, profile }, sidecar) in requests {
            let target = arch.minimal_rt_triple();

            // We use special profiles for boot, convert from the standard ones:
            let profile = match profile {
                SidecarBuildProfile::Debug => BuildProfile::BootDev,
                SidecarBuildProfile::Release => BuildProfile::BootRelease,
            };

            let mut extra_env = crate::common::openhcl_build_env();
            extra_env.insert("RUSTC_BOOTSTRAP".into(), "1".into());

            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "sidecar".into(),
                out_name: "sidecar".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile,
                features: Default::default(),
                target,
                no_split_dbg_info: false,
                extra_env: Some(ReadVar::from_static(extra_env)),
                pre_build_deps: Vec::new(),
                output: v,
            });

            ctx.emit_minor_rust_step("report built sidecar", |ctx| {
                let sidecar = sidecar.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            SidecarOutput {
                                bin,
                                dbg: dbg.unwrap(),
                            }
                        }
                        _ => unreachable!(),
                    };

                    rt.write_all(sidecar, &output);
                }
            });
        }

        Ok(())
    }
}
