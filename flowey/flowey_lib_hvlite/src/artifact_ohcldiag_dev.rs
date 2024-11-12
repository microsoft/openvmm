// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `ohcldiag-dev` executable + debug symbols.
//!
//! Content varies depending on what platform `ohcldiag-dev` was compiled for.

/// Publish the artifact.
pub mod publish {
    use crate::build_ohcldiag_dev::OhcldiagDevOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub ohcldiag_dev: ReadVar<OhcldiagDevOutput>,
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
                ohcldiag_dev,
                artifact_dir,
                done,
            } = request;

            let files = ohcldiag_dev.map(ctx, |o| match o {
                OhcldiagDevOutput::LinuxBin { bin, dbg } => {
                    vec![
                        ("ohcldiag-dev".into(), bin),
                        ("ohcldiag-dev.dbg".into(), dbg),
                    ]
                }
                OhcldiagDevOutput::WindowsBin { exe, pdb } => {
                    vec![
                        ("ohcldiag-dev.exe".into(), exe),
                        ("ohcldiag_dev.pdb".into(), pdb),
                    ]
                }
            });
            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "ohcldiag-dev".into(),
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
    use crate::build_ohcldiag_dev::OhcldiagDevOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub artifact_dir: ReadVar<PathBuf>,
            pub ohcldiag_dev: WriteVar<OhcldiagDevOutput>,
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
                ohcldiag_dev,
            } = request;

            ctx.emit_rust_step("resolve ohcldiag-dev artifact", |ctx| {
                let artifact_dir = artifact_dir.claim(ctx);
                let ohcldiag_dev = ohcldiag_dev.claim(ctx);
                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);

                    let output = if artifact_dir.join("ohcldiag-dev").exists() {
                        OhcldiagDevOutput::LinuxBin {
                            bin: artifact_dir.join("ohcldiag-dev"),
                            dbg: artifact_dir.join("ohcldiag-dev.dbg"),
                        }
                    } else if artifact_dir.join("ohcldiag-dev.exe").exists()
                        && artifact_dir.join("ohcldiag_dev.pdb").exists()
                    {
                        OhcldiagDevOutput::WindowsBin {
                            exe: artifact_dir.join("ohcldiag-dev.exe"),
                            pdb: artifact_dir.join("ohcldiag_dev.pdb"),
                        }
                    } else {
                        anyhow::bail!("malformed artifact! did not find ohcldiag-dev executable")
                    };

                    rt.write(ohcldiag_dev, &output);

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
