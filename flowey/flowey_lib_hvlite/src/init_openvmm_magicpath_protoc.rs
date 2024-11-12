// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure protoc is symlinked into the correct "magic directory" set by the
//! project-level `[env]` table in `.cargo/config.toml`

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request(pub WriteVar<SideEffect>);
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_openvmm_magicpath::Node>();
        ctx.import::<flowey_lib_common::download_protoc::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let protoc_pkg = ctx.reqv(flowey_lib_common::download_protoc::Request::Get);
        let openvmm_magicpath = ctx.reqv(crate::cfg_openvmm_magicpath::Request);

        ctx.emit_rust_step("symlink protoc", move |ctx| {
            requests.into_iter().for_each(|x| {
                x.0.claim(ctx);
            });
            let protoc_pkg = protoc_pkg.claim(ctx);
            let openvmm_magicpath = openvmm_magicpath.claim(ctx);
            move |rt| {
                let expected_protoc_bin = {
                    if cfg!(target_os = "windows") {
                        "protoc.exe"
                    } else if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
                        "protoc"
                    } else {
                        unreachable!("unknown host os")
                    }
                };

                let openvmm_magicpath = rt.read(openvmm_magicpath);
                let dst_folder = openvmm_magicpath.join("Google.Protobuf.Tools/tools");
                fs_err::create_dir_all(&dst_folder)?;

                let protoc_pkg = rt.read(protoc_pkg);

                flowey_lib_common::_util::copy_dir_all(
                    protoc_pkg.include_dir,
                    openvmm_magicpath.join("Google.Protobuf.Tools/tools/include"),
                )?;

                let src = protoc_pkg.protoc_bin;
                let dst = dst_folder.join(expected_protoc_bin);

                let _ = fs_err::remove_file(&dst);

                if !dst.exists() {
                    fs_err::hard_link(src.clone(), &dst)?;

                    // Make sure protoc is executable
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let old_mode = dst.metadata()?.permissions().mode();
                        fs_err::set_permissions(
                            src,
                            std::fs::Permissions::from_mode(old_mode | 0o111),
                        )?;
                    }
                }

                Ok(())
            }
        });

        Ok(())
    }
}
