// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `rustdoc` documentation (compiled to HTML)
//!
//! Content varies depending on what docs directory was provided.

/// Publish the artifact.
pub mod publish {
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub rustdocs_dir: ReadVar<PathBuf>,
            pub artifact_dir: ReadVar<PathBuf>,
            pub done: WriteVar<SideEffect>,
        }
    }

    new_simple_flow_node!(struct Node);

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(ctx: &mut ImportCtx<'_>) {
            ctx.import::<flowey_lib_common::copy_to_artifact_dir::Node>();
        }

        fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            let Request {
                rustdocs_dir,
                artifact_dir,
                done,
            } = request;

            // Zip the rustdoc directory since there are too many files to upload directly
            let archive = ctx.emit_rust_stepv("zip rustdoc dir", |ctx| {
                let rustdocs_dir = rustdocs_dir.claim(ctx);
                |rt| {
                    let rustdocs_dir = rt.read(rustdocs_dir);
                    let sh = xshell::Shell::new()?;
                    // use in-box tar, which supports zip. works for all
                    // windows builds past Windows 10 build 17063
                    xshell::cmd!(sh, "tar -a -c -f rustdoc.zip {rustdocs_dir}").run()?;
                    Ok(sh.current_dir().join("rustdoc.zip"))
                }
            });

            let files = archive.map(ctx, |p| vec![("docs".into(), p)]);
            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "rustdoc".into(),
                files,
                artifact_dir,
                done,
            });

            Ok(())
        }
    }
}
