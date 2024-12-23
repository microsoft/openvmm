// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `hypestv` executable + debug symbols.

/// Publish the artifact.
pub mod publish {
    use crate::build_hypestv::HypestvOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub hypestv: ReadVar<HypestvOutput>,
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
                hypestv,
                artifact_dir,
                done,
            } = request;

            let files = hypestv.map(ctx, |o| {
                vec![("hypestv.exe".into(), o.exe), ("hypestv.pdb".into(), o.pdb)]
            });

            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "hypestv".into(),
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
    use crate::build_hypestv::HypestvOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub artifact_dir: ReadVar<PathBuf>,
            pub hypestv: WriteVar<HypestvOutput>,
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
                hypestv,
            } = request;

            ctx.emit_rust_step("resolve hypestv artifact", |ctx| {
                let artifact_dir = artifact_dir.claim(ctx);
                let hypestv = hypestv.claim(ctx);
                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);

                    let output = if artifact_dir.join("hypestv.exe").exists()
                        && artifact_dir.join("hypestv.pdb").exists()
                    {
                        HypestvOutput {
                            exe: artifact_dir.join("hypestv.exe"),
                            pdb: artifact_dir.join("hypestv.pdb"),
                        }
                    } else {
                        anyhow::bail!("malformed artifact! did not find hypestv executable")
                    };

                    rt.write(hypestv, &output);

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
