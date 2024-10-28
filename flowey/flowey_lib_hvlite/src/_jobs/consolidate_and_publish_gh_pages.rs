// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Consolidate various pre-built HTML artifacts (guide, docs, etc...), and glue
//! them together into a single HTML bundle which can be published to
//! `openvmm.dev` via gh pages.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub rustdoc_linux: ReadVar<PathBuf>,
        pub rustdoc_windows: ReadVar<PathBuf>,
        pub guide: ReadVar<PathBuf>,

        // optionally pass in an artifact_dir publish the gh pages output to an
        // artifact (e.g: useful for local testing).
        pub artifact_dir: Option<ReadVar<PathBuf>>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_guide::resolve::Node>();
        ctx.import::<crate::artifact_rustdoc::resolve::Node>();
        ctx.import::<flowey_lib_common::copy_to_artifact_dir::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            rustdoc_linux,
            rustdoc_windows,
            guide,
            artifact_dir,
            done,
        } = request;

        let rendered_guide = ctx.reqv(|v| crate::artifact_guide::resolve::Request {
            artifact_dir: guide,
            rendered_guide: v,
        });

        let rustdoc_windows = ctx.reqv(|v| crate::artifact_rustdoc::resolve::Request {
            artifact_dir: rustdoc_windows,
            rustdocs_dir: v,
        });

        let rustdoc_linux = ctx.reqv(|v| crate::artifact_rustdoc::resolve::Request {
            artifact_dir: rustdoc_linux,
            rustdocs_dir: v,
        });

        let consolidated_html = ctx.emit_rust_stepv("generate consolidated gh pages html", |ctx| {
            let rendered_guide = rendered_guide.claim(ctx);
            let rustdoc_windows = rustdoc_windows.claim(ctx);
            let rustdoc_linux = rustdoc_linux.claim(ctx);
            |rt| {
                let rendered_guide = rt.read(rendered_guide);
                let rustdoc_windows = rt.read(rustdoc_windows);
                let rustdoc_linux = rt.read(rustdoc_linux);

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
                    rendered_guide,
                    consolidated_html.join("guide"),
                )?;

                // Make rustdocs accessible under `openvmm.dev/rustdoc/{platform}`
                flowey_lib_common::_util::copy_dir_all(
                    rustdoc_windows,
                    consolidated_html.join("rustdoc/windows"),
                )?;
                flowey_lib_common::_util::copy_dir_all(
                    rustdoc_linux,
                    consolidated_html.join("rustdoc/linux"),
                )?;

                // as we do not currently have any form of "landing page",
                // redirect `openvmm.dev` to `openvmm.dev/guide`
                fs_err::write(consolidated_html.join("index.html"), REDIRECT)?;

                Ok(consolidated_html)
            }
        });

        if matches!(ctx.backend(), FlowBackend::Github) {
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

            ctx.emit_side_effect_step([did_deploy], [done]);
        } else {
            if let Some(artifact_dir) = artifact_dir {
                let files = consolidated_html.map(ctx, |p| vec![("gh_pages".into(), p)]);
                ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                    debug_label: "gh pages artifact".into(),
                    artifact_dir,
                    files,
                    done,
                })
            } else {
                ctx.emit_side_effect_step([consolidated_html.into_side_effect()], [done]);
            }
        }

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
