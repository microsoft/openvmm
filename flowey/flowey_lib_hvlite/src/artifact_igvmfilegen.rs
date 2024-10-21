// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `igvmfilegen` executable + debug symbols
//!
//! Content varies depending on what platform `igvmfilegen` was compiled for.

/// Publish the artifact.
pub mod publish {
    use crate::build_igvmfilegen::IgvmfilegenOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub igvmfilegen: ReadVar<IgvmfilegenOutput>,
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
                igvmfilegen,
                artifact_dir,
                done,
            } = request;

            let files = igvmfilegen.map(ctx, |o| match o {
                IgvmfilegenOutput::LinuxBin { bin, dbg } => {
                    vec![("igvmfilegen".into(), bin), ("igvmfilegen.dbg".into(), dbg)]
                }
                IgvmfilegenOutput::WindowsBin { exe, pdb } => {
                    vec![
                        ("igvmfilegen.exe".into(), exe),
                        ("igvmfilegen.pdb".into(), pdb),
                    ]
                }
            });

            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "igvmfilegen".into(),
                files,
                artifact_dir,
                done,
            });

            Ok(())
        }
    }
}
