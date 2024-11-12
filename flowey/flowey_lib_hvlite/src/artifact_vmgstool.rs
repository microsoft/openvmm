// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `vmgstool` executable + debug symbols.
//!
//! Content varies depending on what platform `vmgstool` was compiled for.

/// Publish the artifact.
pub mod publish {
    use crate::build_vmgstool::VmgstoolOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub vmgstool: ReadVar<VmgstoolOutput>,
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
                vmgstool,
                artifact_dir,
                done,
            } = request;

            let files = vmgstool.map(ctx, |o| match o {
                VmgstoolOutput::LinuxBin { bin, dbg } => {
                    vec![("vmgstool".into(), bin), ("vmgstool.dbg".into(), dbg)]
                }
                VmgstoolOutput::WindowsBin { exe, pdb } => {
                    vec![("vmgstool.exe".into(), exe), ("vmgstool.pdb".into(), pdb)]
                }
            });

            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "vmgstool".into(),
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
    use crate::build_vmgstool::VmgstoolOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub artifact_dir: ReadVar<PathBuf>,
            pub vmgstool: WriteVar<VmgstoolOutput>,
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
                vmgstool,
            } = request;

            ctx.emit_rust_step("resolve vmgstool artifact", |ctx| {
                let artifact_dir = artifact_dir.claim(ctx);
                let vmgstool = vmgstool.claim(ctx);
                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);

                    let output = if artifact_dir.join("vmgstool").exists() {
                        VmgstoolOutput::LinuxBin {
                            bin: artifact_dir.join("vmgstool"),
                            dbg: artifact_dir.join("vmgstool.dbg"),
                        }
                    } else if artifact_dir.join("vmgstool.exe").exists()
                        && artifact_dir.join("vmgstool.pdb").exists()
                    {
                        VmgstoolOutput::WindowsBin {
                            exe: artifact_dir.join("vmgstool.exe"),
                            pdb: artifact_dir.join("vmgstool.pdb"),
                        }
                    } else {
                        anyhow::bail!("malformed artifact! did not find vmgstool executable")
                    };

                    rt.write(vmgstool, &output);

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
