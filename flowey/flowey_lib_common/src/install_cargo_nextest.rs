// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Install `cargo-nextest`.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request(pub WriteVar<SideEffect>);
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_rust::Node>();
        ctx.import::<crate::download_cargo_nextest::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut done = Vec::new();

        for req in requests {
            done.push(req.0);
        }

        let done = done;

        // -- end of req processing -- //

        if done.is_empty() {
            return Ok(());
        }

        let cargo_nextest_bin = ctx.platform().binary("cargo-nextest");

        let nextest_path = ctx.reqv(|v| {
            crate::download_cargo_nextest::Request::Get(
                ReadVar::from_static(target_lexicon::Triple::host()),
                v,
            )
        });
        let cargo_home = ctx.reqv(crate::install_rust::Request::GetCargoHome);
        let rust_installed = ctx.reqv(crate::install_rust::Request::EnsureInstalled);

        ctx.emit_rust_step("installing cargo-nextest", |ctx| {
            let nextest_path = nextest_path.claim(ctx);
            let cargo_home = cargo_home.claim(ctx);
            rust_installed.claim(ctx);
            done.claim(ctx);

            move |rt| {
                let nextest_path = rt.read(nextest_path);
                let cargo_home = rt.read(cargo_home);

                // copy to cargo home bin folder so that nextest
                // is accessible via `cargo nextest``
                fs_err::copy(
                    &nextest_path,
                    cargo_home.join("bin").join(&cargo_nextest_bin),
                )?;

                Ok(())
            }
        });

        Ok(())
    }
}
