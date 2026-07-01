// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `igvmfilegen` binaries

use crate::common::CommonTriple;
use crate::run_cargo_build::BuildProfile;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum IgvmfilegenOutput {
    LinuxBin {
        #[serde(rename = "igvmfilegen")]
        bin: PathBuf,
        #[serde(rename = "igvmfilegen.dbg")]
        dbg: PathBuf,
    },
    WindowsBin {
        #[serde(rename = "igvmfilegen.exe")]
        exe: PathBuf,
        #[serde(rename = "igvmfilegen.pdb")]
        pdb: PathBuf,
    },
}

impl Artifact for IgvmfilegenOutput {}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct IgvmfilegenBuildParams {
    pub target: CommonTriple,
    pub profile: BuildProfile,
}

flowey_request! {
    pub struct Request {
        pub build_params: IgvmfilegenBuildParams,
        pub igvmfilegen: WriteVar<IgvmfilegenOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        // de-dupe incoming requests
        let requests = requests
            .into_iter()
            .fold(BTreeMap::<_, Vec<_>>::new(), |mut m, r| {
                let Request {
                    build_params,
                    igvmfilegen,
                } = r;
                m.entry(build_params).or_default().push(igvmfilegen);
                m
            });

        // igvmfilegen depends on the workspace `crypto` crate, which on
        // `linux-gnu` targets pulls in openssl-sys and requires the OpenSSL
        // development headers + pkg-config to be present on the build host.
        let mut pre_build_deps = Vec::new();
        if matches!(
            ctx.platform(),
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu)
        ) {
            pre_build_deps.push(ctx.reqv(|v| {
                flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec!["libssl-dev".into(), "pkg-config".into()],
                    done: v,
                }
            }));
        }

        for (IgvmfilegenBuildParams { target, profile }, outvars) in requests {
            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "igvmfilegen".into(),
                out_name: "igvmfilegen".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile,
                features: Default::default(),
                target: target.as_triple(),
                no_split_dbg_info: false,
                extra_env: None,
                pre_build_deps: pre_build_deps.clone(),
                output: v,
            });

            ctx.emit_minor_rust_step("report built igvmfilegen", |ctx| {
                let outvars = outvars.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                            IgvmfilegenOutput::WindowsBin { exe, pdb }
                        }
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            IgvmfilegenOutput::LinuxBin {
                                bin,
                                dbg: dbg.unwrap(),
                            }
                        }
                        _ => unreachable!(),
                    };

                    for var in outvars {
                        rt.write(var, &output);
                    }
                }
            });
        }

        Ok(())
    }
}
