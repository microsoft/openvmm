// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Consolidate various pre-built HTML artifacts (guide, docs, etc...), and glue
//! them together into a single HTML bundle which can be published to
//! `openvmm.dev` via gh pages.

use crate::build_guide::GuideOutput;
use crate::build_rustdoc::RustdocOutput;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub rustdoc_linux: ReadVar<RustdocOutput>,
        pub rustdoc_windows: ReadVar<RustdocOutput>,
        pub guide: ReadVar<GuideOutput>,
        pub output: WriteVar<GhPagesOutput>,
    }
}

#[derive(Serialize, Deserialize)]
pub struct GhPagesOutput {
    pub gh_pages: PathBuf,
}

impl Artifact for GhPagesOutput {}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::_jobs::build_test_results_website::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            rustdoc_linux,
            rustdoc_windows,
            guide: rendered_guide,
            output,
        } = request;

        let repo = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let logview_new_build =
            ctx.reqv(|v| crate::_jobs::build_test_results_website::Request { path: v });

        let consolidated_html = ctx.emit_rust_stepv("generate consolidated gh pages html", |ctx| {
            let rendered_guide = rendered_guide.claim(ctx);
            let rustdoc_windows = rustdoc_windows.claim(ctx);
            let rustdoc_linux = rustdoc_linux.claim(ctx);
            let repo = repo.claim(ctx);
            let logview_new_build = logview_new_build.claim(ctx);
            |rt| {
                let rendered_guide = rt.read(rendered_guide);
                let rustdoc_windows = rt.read(rustdoc_windows);
                let rustdoc_linux = rt.read(rustdoc_linux);
                let repo = rt.read(repo);
                let logview_new_build = rt.read(logview_new_build);

                let consolidated_html = std::env::current_dir()?.join("out").absolute()?;
                fs_err::create_dir(&consolidated_html)?;

                // DEVNOTE: Please try to keep this top-level structure stable!
                //
                // As the project grows, its quite likely more external websites
                // will be linking to specific pages under `openvmm.dev`. Lets
                // do our best to avoid linkrot, and if we are moving things
                // around, lets make sure to add appropriate redirects whenever
                // we can.

                // Make the OpenVMM Guide accessible under `openvmm.dev/guide/`
                flowey_lib_common::_util::copy_dir_all(
                    rendered_guide.guide,
                    consolidated_html.join("guide"),
                )?;

                // Make rustdocs accessible under `openvmm.dev/rustdoc/{platform}`
                flowey_lib_common::_util::copy_dir_all(
                    rustdoc_windows.docs,
                    consolidated_html.join("rustdoc/windows"),
                )?;
                flowey_lib_common::_util::copy_dir_all(
                    rustdoc_linux.docs,
                    consolidated_html.join("rustdoc/linux"),
                )?;

                // Old logviewer. Keeping this around while the new one is stabilized.
                flowey_lib_common::_util::copy_dir_all(
                    repo.join("petri/logview"),
                    consolidated_html.join("test-results-old"),
                )?;

                // New logviewer (built in CI).
                flowey_lib_common::_util::copy_dir_all(
                    logview_new_build,
                    consolidated_html.join("test-results"),
                )?;

                // as we do not currently have any form of "landing page",
                // redirect `openvmm.dev` to `openvmm.dev/guide`
                fs_err::write(consolidated_html.join("index.html"), REDIRECT)?;

                Ok(consolidated_html)
            }
        });

        let consolidated_html = if matches!(ctx.backend(), FlowBackend::Github) {
            let did_upload = ctx
                .emit_gh_step("Upload pages artifact", "actions/upload-pages-artifact@v3")
                .with(
                    "path",
                    consolidated_html.map(ctx, |x| x.display().to_string()),
                )
                .finish(ctx);

            let did_deploy = ctx
                .emit_gh_step("Deploy to GitHub Pages", "actions/deploy-pages@v4")
                .requires_permission(GhPermission::IdToken, GhPermissionValue::Write)
                .requires_permission(GhPermission::Pages, GhPermissionValue::Write)
                .run_after(did_upload)
                .finish(ctx);

            consolidated_html.depending_on(ctx, &did_deploy)
        } else {
            consolidated_html
        };

        consolidated_html.write_into(ctx, output, |p| GhPagesOutput { gh_pages: p });
        Ok(())
    }
}

const REDIRECT: &str = r#"
<!DOCTYPE html>
<html>
<head>
    <title>Redirecting...</title>
    <link rel="canonical" href="/guide"/>
    <meta charset="utf-8"/>
    <meta http-equiv="refresh" content="0; url=/guide">
</head>
<body>
    <p>If you are not redirected automatically, follow this <a href="/guide">link to openvmm.dev/guide</a>.</p>
</body>
</html>
"#;
