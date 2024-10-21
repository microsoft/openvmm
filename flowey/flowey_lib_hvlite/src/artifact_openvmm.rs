// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `openvmm` executable + debug symbols
//!
//! Content varies depending on what platform `openvmm` was compiled for.

/// Publish the artifact.
pub mod publish {
    use crate::build_openvmm::OpenvmmOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub openvmm: ReadVar<OpenvmmOutput>,
            pub artifact_dir: ReadVar<PathBuf>,
            pub done: WriteVar<SideEffect>,
        }
    }

    new_flow_node!(struct Node);

    impl FlowNode for Node {
        type Request = Request;

        fn imports(_ctx: &mut ImportCtx<'_>) {}

        fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            for Request {
                openvmm,
                artifact_dir,
                done,
            } in requests
            {
                ctx.emit_rust_step("copying openvmm to publish dir", |ctx| {
                    done.claim(ctx);
                    let openvmm = openvmm.claim(ctx);
                    let artifact_dir = artifact_dir.claim(ctx);

                    move |rt| {
                        let openvmm = rt.read(openvmm);
                        let artifact_dir = rt.read(artifact_dir);

                        match openvmm {
                            OpenvmmOutput::WindowsBin { exe, pdb } => {
                                fs_err::copy(exe, artifact_dir.join("openvmm.exe"))?;
                                fs_err::copy(pdb, artifact_dir.join("openvmm.pdb"))?;
                            }
                            OpenvmmOutput::LinuxBin { bin, dbg } => {
                                fs_err::copy(bin, artifact_dir.join("openvmm"))?;
                                fs_err::copy(dbg, artifact_dir.join("openvmm.dbg"))?;
                            }
                        }

                        Ok(())
                    }
                });
            }

            Ok(())
        }
    }
}

/// Resolve the contents of an existing artifact.
pub mod resolve {
    use crate::build_openvmm::OpenvmmOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub artifact_dir: ReadVar<PathBuf>,
            pub openvmm: WriteVar<OpenvmmOutput>,
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
                openvmm,
            } = request;

            ctx.emit_rust_step("resolve openvmm artifact", |ctx| {
                let artifact_dir = artifact_dir.claim(ctx);
                let openvmm = openvmm.claim(ctx);
                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);

                    let output = if artifact_dir.join("openvmm").exists() {
                        OpenvmmOutput::LinuxBin {
                            bin: artifact_dir.join("openvmm"),
                            dbg: artifact_dir.join("openvmm.dbg"),
                        }
                    } else if artifact_dir.join("openvmm.exe").exists()
                        && artifact_dir.join("openvmm.pdb").exists()
                    {
                        OpenvmmOutput::WindowsBin {
                            exe: artifact_dir.join("openvmm.exe"),
                            pdb: artifact_dir.join("openvmm.pdb"),
                        }
                    } else {
                        anyhow::bail!("malformed artifact! did not find openvmm executable")
                    };

                    rt.write(openvmm, &output);

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
