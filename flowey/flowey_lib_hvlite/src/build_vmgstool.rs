// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `vmgstool` binaries

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoCrateType;
use flowey_lib_common::run_cargo_build::CargoFeatureSet;

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum VmgstoolOutput {
    LinuxBin {
        #[serde(rename = "vmgstool")]
        bin: PathBuf,
        #[serde(rename = "vmgstool.dbg")]
        dbg: PathBuf,
    },
    WindowsBin {
        #[serde(rename = "vmgstool.exe")]
        exe: PathBuf,
        #[serde(rename = "vmgstool.pdb")]
        pdb: PathBuf,
    },
}

impl Artifact for VmgstoolOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub with_crypto: bool,
        pub with_test_helpers: bool,
        pub vmgstool: WriteVar<VmgstoolOutput>,
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
            with_crypto,
            with_test_helpers,
            vmgstool,
        } = request;

        let mut pre_build_deps = Vec::new();

        if with_crypto {
            pre_build_deps.push(ctx.reqv(|v| {
                flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec!["libssl-dev".into()],
                    done: v,
                }
            }));
        }

        let mut features = Vec::new();
        if with_crypto {
            match target.as_triple().operating_system {
                target_lexicon::OperatingSystem::Windows => features.push("encryption_win".into()),
                target_lexicon::OperatingSystem::Linux => features.push("encryption_ossl".into()),
                _ => unreachable!(),
            };
        }
        if with_test_helpers {
            features.push("test_helpers".into());
        }

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "vmgstool".into(),
            out_name: "vmgstool".into(),
            crate_type: CargoCrateType::Bin,
            profile: profile.into(),
            features: CargoFeatureSet::Specific(features),
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps,
            output: v,
        });

        ctx.emit_minor_rust_step("report built vmgstool", |ctx| {
            let vmgstool = vmgstool.claim(ctx);
            let output = output.claim(ctx);
            move |rt| {
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::WindowsBin { exe, pdb } => {
                        VmgstoolOutput::WindowsBin { exe, pdb }
                    }
                    crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                        VmgstoolOutput::LinuxBin {
                            bin,
                            dbg: dbg.unwrap(),
                        }
                    }
                    _ => unreachable!(),
                };

                rt.write(vmgstool, &output);
            }
        });

        Ok(())
    }
}
