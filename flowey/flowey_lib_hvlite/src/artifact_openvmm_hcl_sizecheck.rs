// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `openhcl` binary to use for PR binary size comparison

/// Publish the artifact.
pub mod publish {
    use crate::build_openvmm_hcl::OpenvmmHclOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub openvmm_openhcl_x86: ReadVar<OpenvmmHclOutput>,
            pub artifact_dir: ReadVar<PathBuf>,
            pub done: WriteVar<SideEffect>,
        }
    }

    new_simple_flow_node!(struct Node);

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(_ctx: &mut ImportCtx<'_>) {}

        fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            let Request {
                openvmm_openhcl_x86,
                artifact_dir,
                done,
            } = request;

            ctx.emit_rust_step("copying openhcl build to publish dir", |ctx| {
                done.claim(ctx);
                let artifact_dir = artifact_dir.claim(ctx);
                let openvmm_openhcl_x86 = openvmm_openhcl_x86.claim(ctx);

                move |rt| {
                    let artifact_dir = rt.read(artifact_dir);
                    let openvmm_openhcl_x86 = rt.read(openvmm_openhcl_x86);
                    fs_err::copy(openvmm_openhcl_x86.bin, artifact_dir.join("openhcl"))?;

                    Ok(())
                }
            });

            Ok(())
        }
    }
}
