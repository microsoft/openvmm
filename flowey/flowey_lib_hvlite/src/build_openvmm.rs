// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `openvmm` binaries

use crate::download_lxutil::LxutilArch;
use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use std::collections::BTreeSet;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OpenvmmFeature {
    Gdb,
    Tpm,
    UnstableWhp,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OpenvmmBuildParams {
    pub profile: CommonProfile,
    pub target: CommonTriple,
    pub features: BTreeSet<OpenvmmFeature>,
}

#[derive(Serialize, Deserialize)]
pub enum OpenvmmOutput {
    WindowsBin { exe: PathBuf, pdb: PathBuf },
    LinuxBin { bin: PathBuf, dbg: PathBuf },
}

flowey_request! {
    pub struct Request {
        pub params: OpenvmmBuildParams,
        pub openvmm: WriteVar<OpenvmmOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::init_openvmm_magicpath_lxutil::Node>();
        ctx.import::<crate::run_cargo_build::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let installed_apt_deps =
            ctx.reqv(|v| flowey_lib_common::install_dist_pkg::Request::Install {
                package_names: vec!["libssl-dev".into(), "build-essential".into()],
                done: v,
            });

        for Request {
            params:
                OpenvmmBuildParams {
                    profile,
                    target,
                    features,
                },
            openvmm: openvmm_bin,
        } in requests
        {
            let mut pre_build_deps = vec![installed_apt_deps.clone()];

            let lxutil_arch = match target.as_triple().architecture {
                target_lexicon::Architecture::Aarch64(_) => LxutilArch::Aarch64,
                target_lexicon::Architecture::X86_64 => LxutilArch::X86_64,
                arch => anyhow::bail!("no lxutil package for specified arch: {:?}", arch),
            };

            // NOTE: OpenVMM's code is currently hard-coded to assume lxutil
            // package is in a particular place
            pre_build_deps.push(ctx.reqv(|v| crate::init_openvmm_magicpath_lxutil::Request {
                arch: lxutil_arch,
                done: v,
            }));

            // TODO: also need to take into account any default features in
            // openvmm's Cargo.toml?
            //
            // maybe we can do something clever and parse the openvmm Cargo.toml
            // file to discover these defaults?
            for feat in &features {
                match feat {
                    OpenvmmFeature::Gdb => {}
                    OpenvmmFeature::Tpm => pre_build_deps.push(ctx.reqv(|v| {
                        flowey_lib_common::install_dist_pkg::Request::Install {
                            package_names: vec!["build-essential".into()],
                            done: v,
                        }
                    })),
                    OpenvmmFeature::UnstableWhp => {}
                }
            }

            let output = ctx.reqv(|v| crate::run_cargo_build::Request {
                crate_name: "openvmm".into(),
                out_name: "openvmm".into(),
                crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
                profile: profile.into(),
                features: features
                    .into_iter()
                    .map(|f| {
                        match f {
                            OpenvmmFeature::Gdb => "gdb",
                            OpenvmmFeature::Tpm => "tpm",
                            OpenvmmFeature::UnstableWhp => "unstable_whp",
                        }
                        .into()
                    })
                    .collect(),
                target: target.as_triple(),
                no_split_dbg_info: false,
                extra_env: None,
                pre_build_deps,
                output: v,
            });

            ctx.emit_rust_step("report built openvmm", |ctx| {
                let openvmm_bin = openvmm_bin.claim(ctx);
                let output = output.claim(ctx);
                move |rt| {
                    let output = match rt.read(output) {
                        crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                            OpenvmmOutput::WindowsBin { exe, pdb }
                        }
                        crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                            OpenvmmOutput::LinuxBin {
                                bin,
                                dbg: dbg.unwrap(),
                            }
                        }
                        _ => unreachable!(),
                    };

                    rt.write(openvmm_bin, &output);

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
