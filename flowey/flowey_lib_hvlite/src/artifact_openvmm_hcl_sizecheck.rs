// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Artifact: `openhcl` binary to use for PR binary size comparison

/// Publish the artifact.
pub mod publish {
    use crate::artifact_openhcl_igvm_from_recipe::recipe_to_filename;
    use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
    use crate::build_openvmm_hcl::OpenvmmHclOutput;
    use flowey::node::prelude::*;

    flowey_request! {
        pub struct Request {
            pub openhcl_builds: Vec<(OpenhclIgvmRecipe, ReadVar<OpenvmmHclOutput>)>,
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
                openhcl_builds,
                artifact_dir,
                done,
            } in requests
            {
                ctx.emit_rust_step("copying openhcl builds to publish dir", |ctx| {
                    done.claim(ctx);
                    let artifact_dir = artifact_dir.claim(ctx);
                    let openhcl_builds = openhcl_builds
                        .iter()
                        .map(|x| (x.0.clone(), x.1.clone().claim(ctx)))
                        .collect::<Vec<_>>();

                    move |rt| {
                        let artifact_dir = rt.read(artifact_dir);
                        for (recipe, build) in openhcl_builds {
                            let build = rt.read(build);
                            fs_err::copy(
                                build.bin,
                                artifact_dir.join(recipe_to_filename(&recipe)),
                            )?;
                        }

                        Ok(())
                    }
                });
            }

            Ok(())
        }
    }
}
