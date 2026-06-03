// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Copy virtio-win driver files into the `.packages/virtio-win/` magicpath
//! directory so that `cargo test` and `cargo run -p prep_steps` can find them
//! without going through flowey.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request(pub WriteVar<SideEffect>);
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_openvmm_magicpath::Node>();
        ctx.import::<crate::resolve_openvmm_test_virtio_win::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request(done) = request;
        let magicpath = ctx.reqv(crate::cfg_openvmm_magicpath::Request);
        let virtio_win_dir = ctx.reqv(crate::resolve_openvmm_test_virtio_win::Request::Get);

        ctx.emit_rust_step("copy virtio-win drivers to magicpath", |ctx| {
            let magicpath = magicpath.claim(ctx);
            let virtio_win_dir = virtio_win_dir.claim(ctx);
            done.claim(ctx);
            move |rt| {
                let magicpath = rt.read(magicpath);
                let src = rt.read(virtio_win_dir);
                let dst = magicpath.join("virtio-win");
                let _ = fs_err::remove_dir_all(&dst);
                flowey_lib_common::_util::copy_dir_all(&src, &dst)?;
                Ok(())
            }
        });

        Ok(())
    }
}
