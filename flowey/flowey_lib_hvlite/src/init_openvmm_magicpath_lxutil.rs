// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure the lxutil package is moved into the correct "magic directory"
//! as expected by the project-level `[env]` table in `.cargo/config.toml`

use crate::download_lxutil::LxutilArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        pub arch: LxutilArch,
        pub done: WriteVar<SideEffect>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_openvmm_magicpath::Node>();
        ctx.import::<crate::download_lxutil::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut reqs: BTreeMap<LxutilArch, Vec<WriteVar<SideEffect>>> = BTreeMap::new();
        for Request { arch, done } in requests {
            reqs.entry(arch).or_default().push(done);
        }

        // -- end of req processing -- //

        let packages = reqs
            .into_iter()
            .map(|(arch, dones)| {
                (
                    arch,
                    (
                        ctx.reqv(|v| crate::download_lxutil::Request::GetPackage { arch, pkg: v }),
                        dones,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();

        let openvmm_magicpath = ctx.reqv(crate::cfg_openvmm_magicpath::Request);

        ctx.emit_rust_step("move lxutil.dll into its magic folder", |ctx| {
            let packages = packages.claim(ctx);
            let openvmm_magicpath = openvmm_magicpath.claim(ctx);
            |rt| {
                let openvmm_magicpath = rt.read(openvmm_magicpath);
                for (arch, (pkg, _dones)) in packages {
                    let pkg = rt.read(pkg);
                    let dst_folder = openvmm_magicpath
                        .join(format!(
                            "Microsoft.WSL.LxUtil.{}",
                            match arch {
                                LxutilArch::Aarch64 => "arm64fre",
                                LxutilArch::X86_64 => "amd64fre",
                            }
                        ))
                        .join("build/native/bin");
                    let dst = dst_folder.join("lxutil.dll");

                    if pkg.lxutil_dll.absolute()? != dst.absolute()? {
                        fs_err::create_dir_all(&dst_folder)?;
                        fs_err::copy(pkg.lxutil_dll, dst_folder.join("lxutil.dll"))?;
                    }
                }
                Ok(())
            }
        });

        Ok(())
    }
}
