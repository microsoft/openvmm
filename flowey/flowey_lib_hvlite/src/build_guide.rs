// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the HvLite Guide

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub guide_source: ReadVar<PathBuf>,
        pub built_guide: WriteVar<PathBuf>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::download_mdbook::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mdbook_bin = ctx.reqv(flowey_lib_common::download_mdbook::Request::GetMdbook);
        for Request {
            built_guide,
            guide_source,
        } in requests
        {
            ctx.emit_rust_step("build HvLite guide (mdbook)", |ctx| {
                let built_guide = built_guide.claim(ctx);
                let guide_source = guide_source.claim(ctx);
                let mdbook_bin = mdbook_bin.clone().claim(ctx);

                |rt| {
                    let mdbook_bin = rt.read(mdbook_bin);
                    let sh = xshell::Shell::new()?;

                    let out_path: PathBuf = sh.current_dir().absolute()?.join("book");
                    let guide_source: PathBuf = rt.read(guide_source);

                    xshell::cmd!(
                        sh,
                        "{mdbook_bin} build {guide_source} --dest-dir {out_path}"
                    )
                    .run()?;

                    rt.write(built_guide, &out_path);

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
