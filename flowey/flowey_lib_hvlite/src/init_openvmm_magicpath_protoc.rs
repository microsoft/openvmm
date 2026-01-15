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
        ctx.import::<flowey_lib_common::resolve_protoc::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let protoc_pkg = ctx.reqv(flowey_lib_common::resolve_protoc::Request::Get);
        let openvmm_magicpath = ctx.reqv(crate::cfg_openvmm_magicpath::Request);

        ctx.emit_rust_step("symlink protoc", move |ctx| {
            requests.into_iter().for_each(|x| {
                x.0.claim(ctx);
            });
            let protoc_pkg = protoc_pkg.claim(ctx);
            let openvmm_magicpath = openvmm_magicpath.claim(ctx);
            move |rt| {
                let expected_protoc_bin = {
                    match rt.platform() {
                        FlowPlatform::Windows => "protoc.exe",
                        FlowPlatform::MacOs | FlowPlatform::Linux(_) => "protoc",
                        _ => unreachable!("unknown host os"),
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
                    dst.make_executable()?;
                }

                Ok(())
            }
        });

        Ok(())
    }
}
