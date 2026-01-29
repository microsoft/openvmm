// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A configuration node that configures the Rust toolchain version to use in
//! OpenVMM pipelines. Having a separate node dedicated for this allows us to
//! patch this node internally where the rustup toolchain is not available.
//! This node also allows us to decouple the rustup version used in oss/internal.

use flowey::node::prelude::*;

pub const RUSTUP_TOOLCHAIN: &str = "1.93.0";

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_rust::Node>();
    }

    fn process_request(_: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        ctx.req(flowey_lib_common::install_rust::Request::Version(
            RUSTUP_TOOLCHAIN.into(),
        ));
        Ok(())
    }
}

flowey_request! {
    pub enum Request {
        Init,
    }
}
