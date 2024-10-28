// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `rustdoc` documentation, compiled to HTML, and compressed as a
//! `tar.gz` file`.

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

            // compress the rustdoc directory since there are too many files to upload directly
            let archive = ctx.emit_rust_stepv("archive rustdoc dir", |ctx| {
                let rustdocs_dir = rustdocs_dir.claim(ctx);
                |rt| {
                    let rustdocs_dir = rt.read(rustdocs_dir);
                    let sh = xshell::Shell::new()?;
                    // use in-box tar. works for all windows builds past Windows
                    // 10 build 17063
                    xshell::cmd!(sh, "tar -a -c -f rustdoc.tar.gz -C {rustdocs_dir} .").run()?;
                    Ok(sh.current_dir().join("rustdoc.tar.gz"))
                }
            });

            let files = archive.map(ctx, |p| vec![("rustdoc.tar.gz".into(), p)]);
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

/// Resolve the contents of an existing artifact.
pub mod resolve {
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub artifact_dir: ReadVar<PathBuf>,
            pub rustdocs_dir: WriteVar<PathBuf>,
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
                artifact_dir,
                rustdocs_dir,
            } = request;

            ctx.emit_rust_step("resolve rustdocs artifact", |ctx| {
                let artifact_dir = artifact_dir.claim(ctx);
                let rustdocs_dir = rustdocs_dir.claim(ctx);
                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);

                    let archive = artifact_dir.join("rustdoc.tar.gz");
                    let output = std::env::current_dir()?.join("docs").absolute()?;

                    let sh = xshell::Shell::new()?;
                    // use in-box tar. works for all windows builds past Windows
                    // 10 build 17063
                    fs_err::create_dir(&output)?;
                    xshell::cmd!(sh, "tar -x -f {archive} -C {output}").run()?;

                    rt.write(rustdocs_dir, &output);

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
