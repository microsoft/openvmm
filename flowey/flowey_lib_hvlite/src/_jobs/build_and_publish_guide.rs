// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and publish the OpenVMM Guide using `mdbook`

use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
        pub deploy_github_pages: bool,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::download_mdbook::Node>();
        ctx.import::<crate::artifact_guide::publish::Node>();
        ctx.import::<crate::build_guide::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            artifact_dir,
            done,
            deploy_github_pages,
        } = request;

        let guide_source = ctx
            .reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir)
            .map(ctx, |p| p.join("Guide"));

        let rendered_guide = ctx.reqv(|v| crate::build_guide::Request {
            guide_source,
            built_guide: v,
        });

        let did_publish_artifact = ctx.reqv(|v| crate::artifact_guide::publish::Request {
            rendered_guide: rendered_guide.clone(),
            artifact_dir,
            done: v,
        });

        if deploy_github_pages && matches!(ctx.backend(), FlowBackend::Github) {
            let did_upload = ctx
                .emit_gh_step("Upload pages artifact", "actions/upload-pages-artifact@v1")
                .with("path", rendered_guide.map(ctx, |x| x.display().to_string()))
                .finish(ctx);

            let did_deploy = ctx
                .emit_gh_step("Deploy to GitHub Pages", "actions/deploy-pages@v3")
                .run_after(did_upload)
                .finish(ctx);

            ctx.emit_side_effect_step([did_publish_artifact, did_deploy], [done]);
        } else {
            ctx.emit_side_effect_step([did_publish_artifact], [done]);
        }

        Ok(())
    }
}
