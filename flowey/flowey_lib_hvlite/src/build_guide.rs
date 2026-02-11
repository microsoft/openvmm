// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the OpenVMM Guide.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub built_guide: WriteVar<GuideOutput>,
    }
}

#[derive(Serialize, Deserialize)]
pub struct GuideOutput {
    #[serde(rename = "Guide")]
    pub guide: PathBuf,
}

impl Artifact for GuideOutput {}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::download_mdbook_admonish::Node>();
        ctx.import::<flowey_lib_common::download_mdbook_mermaid::Node>();
        ctx.import::<flowey_lib_common::download_mdbook::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mdbook_bin = ctx.reqv(flowey_lib_common::download_mdbook::Request::GetMdbook);
        let mdbook_admonish_bin =
            ctx.reqv(flowey_lib_common::download_mdbook_admonish::Request::GetMdbookAdmonish);
        let mdbook_mermaid_bin =
            ctx.reqv(flowey_lib_common::download_mdbook_mermaid::Request::GetMdbookMermaid);

        let guide_source = ctx
            .reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir)
            .map(ctx, |p| p.join("Guide"));

        let rust_is_installed = ctx.reqv(flowey_lib_common::install_rust::Request::EnsureInstalled);

        for Request { built_guide } in requests {
            ctx.emit_rust_step("build OpenVMM guide (mdbook)", |ctx| {
                // rust must be installed to build the `mdbook-openvmm-shim`
                rust_is_installed.clone().claim(ctx);
                let mdbook_bin = mdbook_bin.clone().claim(ctx);
                let mdbook_admonish_bin = mdbook_admonish_bin.clone().claim(ctx);
                let mdbook_mermaid_bin = mdbook_mermaid_bin.clone().claim(ctx);
                let built_guide = built_guide.claim(ctx);
                let guide_source = guide_source.clone().claim(ctx);
                |rt| {
                    let mdbook_bin = rt.read(mdbook_bin);
                    let mdbook_admonish_bin = rt.read(mdbook_admonish_bin);
                    let mdbook_mermaid_bin = rt.read(mdbook_mermaid_bin);

                    let out_path: PathBuf = rt.sh.current_dir().absolute()?.join("book");
                    let guide_source: PathBuf = rt.read(guide_source);

                    rt.sh.change_dir(&guide_source);
                    flowey::shell_cmd!(
                        rt,
                        "{mdbook_bin} build {guide_source} --dest-dir {out_path}"
                    )
                    // intercepted by the `mdbook-openvmm-shim`
                    .env("SHIM_MDBOOK_ADMONISH", mdbook_admonish_bin)
                    .env("SHIM_MDBOOK_MERMAID", mdbook_mermaid_bin)
                    .run()?;

                    rt.write(built_guide, &GuideOutput { guide: out_path });

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
