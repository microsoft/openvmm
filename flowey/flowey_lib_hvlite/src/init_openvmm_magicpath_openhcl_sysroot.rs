// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure the OpenHCL sysroot is extracted into the correct "magic directory"
//! set by the project-level `[env]` table in `.cargo/config.toml`

use crate::resolve_openvmm_deps::OpenvmmDepsArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

new_flow_node!(struct Node);

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum OpenvmmSysrootArch {
    Aarch64,
    X64,
}

flowey_request! {
    pub struct Request {
        pub arch: OpenvmmSysrootArch,
        pub path: WriteVar<PathBuf>,
    }
}

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_openvmm_magicpath::Node>();
        ctx.import::<crate::resolve_openvmm_deps::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut sysroot_arch: BTreeMap<_, Vec<_>> = BTreeMap::new();

        for Request { arch, path } in requests {
            sysroot_arch.entry(arch).or_default().push(path)
        }

        let sysroot_arch = sysroot_arch;

        // -- end of req processing -- //

        if !matches!(ctx.platform(), FlowPlatform::Linux(_)) {
            anyhow::bail!("step only available on linux systems");
        }

        let openvmm_magicpath = ctx.reqv(crate::cfg_openvmm_magicpath::Request);

        for (arch, out_vars) in sysroot_arch {
            let openhcl_sysroot_tar_gz = ctx.reqv(|v| {
                crate::resolve_openvmm_deps::Request::GetOpenhclSysroot(
                    match arch {
                        OpenvmmSysrootArch::Aarch64 => OpenvmmDepsArch::Aarch64,
                        OpenvmmSysrootArch::X64 => OpenvmmDepsArch::X86_64,
                    },
                    v,
                )
            });

            let openvmm_magicpath = openvmm_magicpath.clone();

            // TODO: Refactor this into using a `flowey_lib_common::_util::extract` helper
            ctx.emit_rust_step(format!("extract {arch:?} sysroot.tar.gz"), move |ctx| {
                let openhcl_sysroot_tar_gz = openhcl_sysroot_tar_gz.claim(ctx);
                let openvmm_magicpath = openvmm_magicpath.claim(ctx);
                let requests = out_vars.claim(ctx);

                move |rt| {
                    let openhcl_sysroot_tar_gz = rt.read(openhcl_sysroot_tar_gz);

                    let extracted_sysroot_path =
                        rt.read(openvmm_magicpath)
                            .join("extracted")
                            .join(match arch {
                                OpenvmmSysrootArch::Aarch64 => "aarch64-sysroot",
                                OpenvmmSysrootArch::X64 => "x86_64-sysroot",
                            });
                    fs_err::create_dir_all(&extracted_sysroot_path)?;

                    flowey::shell_cmd!(
                        rt,
                        "tar
                                -xf {openhcl_sysroot_tar_gz}
                                -C {extracted_sysroot_path}
                                --no-same-owner
                                --no-same-permissions
                            "
                    )
                    .run()?;

                    for var in requests {
                        rt.write(var, &extracted_sysroot_path.absolute()?)
                    }

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
