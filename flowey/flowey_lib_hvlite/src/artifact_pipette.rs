// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `pipette` executable + debug symbols.
//!
//! Content varies depending on what platform `pipette` was compiled for.

/// Publish the artifact.
pub mod publish {
    use crate::build_pipette::PipetteOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub pipette: ReadVar<PipetteOutput>,
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
                pipette,
                artifact_dir,
                done,
            } = request;

            let files = pipette.map(ctx, |o| match o {
                PipetteOutput::LinuxBin { bin, dbg } => {
                    vec![("pipette".into(), bin), ("pipette.dbg".into(), dbg)]
                }
                PipetteOutput::WindowsBin { exe, pdb } => {
                    vec![("pipette.exe".into(), exe), ("pipette.pdb".into(), pdb)]
                }
            });
            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "pipette".into(),
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
    use crate::build_pipette::PipetteOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub artifact_dir: ReadVar<PathBuf>,
            pub pipette: WriteVar<PipetteOutput>,
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
                pipette,
            } = request;

            ctx.emit_rust_step("resolve pipette artifact", |ctx| {
                let artifact_dir = artifact_dir.claim(ctx);
                let pipette = pipette.claim(ctx);
                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);

                    let output = if artifact_dir.join("pipette").exists() {
                        PipetteOutput::LinuxBin {
                            bin: artifact_dir.join("pipette"),
                            dbg: artifact_dir.join("pipette.dbg"),
                        }
                    } else if artifact_dir.join("pipette.exe").exists()
                        && artifact_dir.join("pipette.pdb").exists()
                    {
                        PipetteOutput::WindowsBin {
                            exe: artifact_dir.join("pipette.exe"),
                            pdb: artifact_dir.join("pipette.pdb"),
                        }
                    } else {
                        anyhow::bail!("malformed artifact! did not find pipette executable")
                    };

                    rt.write(pipette, &output);

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
