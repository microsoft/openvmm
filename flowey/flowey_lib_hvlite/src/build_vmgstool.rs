// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build `vmgstool` binaries

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoCrateType;

#[derive(Serialize, Deserialize)]
pub enum VmgstoolOutput {
    LinuxBin { bin: PathBuf, dbg: PathBuf },
    WindowsBin { exe: PathBuf, pdb: PathBuf },
}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub with_crypto: bool,
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

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "vmgstool".into(),
            out_name: "vmgstool".into(),
            crate_type: CargoCrateType::Bin,
            profile: profile.into(),
            features: if with_crypto {
                match target.as_triple().operating_system {
                    target_lexicon::OperatingSystem::Windows => ["encryption_win".into()].into(),
                    target_lexicon::OperatingSystem::Linux => ["encryption_ossl".into()].into(),
                    _ => unreachable!(),
                }
            } else {
                [].into()
            },
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps,
            output: v,
        });

        ctx.emit_rust_step("report built vmgstool", |ctx| {
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

                Ok(())
            }
        });

        Ok(())
    }
}
