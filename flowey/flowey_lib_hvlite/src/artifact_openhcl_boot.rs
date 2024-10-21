// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `openhcl_boot` executable + debug symbols

/// Publish the artifact.
pub mod publish {
    use crate::build_openhcl_boot::OpenhclBootOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub openhcl_boot_bin: ReadVar<OpenhclBootOutput>,
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
                openhcl_boot_bin,
                artifact_dir,
                done,
            } = request;

            let files = openhcl_boot_bin.map(ctx, |OpenhclBootOutput { bin, dbg }| {
                vec![
                    ("openhcl_boot".into(), bin),
                    ("openhcl_boot.dbg".into(), dbg),
                ]
            });

            ctx.req(flowey_lib_common::copy_to_artifact_dir::Request {
                debug_label: "openhcl_boot".into(),
                files,
                artifact_dir,
                done,
            });

            Ok(())
        }
    }
}
