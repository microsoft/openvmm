// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Job node that compares two IGVM artifact directories for byte-identical
//! `.bin` files. Used to verify that the node-based reproducible build and the
//! local `cargo xflowey build-reproducible` pipeline produce identical output.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// Path to the IGVM artifact from the node-based build.
        pub artifact_dir_a: ReadVar<PathBuf>,
        /// Path to the IGVM artifact from the local build-reproducible pipeline.
        pub artifact_dir_b: ReadVar<PathBuf>,
        /// File names to compare.
        pub file_names: Vec<String>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            artifact_dir_a,
            artifact_dir_b,
            file_names,
            done,
        } = request;

        let step = ctx.emit_rust_step("compare IGVM binaries", |ctx| {
            let artifact_dir_a = artifact_dir_a.claim(ctx);
            let artifact_dir_b = artifact_dir_b.claim(ctx);
            move |rt| {
                let dir_a = rt.read(artifact_dir_a);
                let dir_b = rt.read(artifact_dir_b);

                log::info!("comparing artifacts:");
                log::info!("  dir a: {}", dir_a.display());
                log::info!("  dir b: {}", dir_b.display());

                for name in &file_names {
                    let bytes_a = fs_err::read(dir_a.join(name))?;
                    let bytes_b = fs_err::read(dir_b.join(name))?;

                    if bytes_a != bytes_b {
                        let first_diff =
                            bytes_a.iter().zip(bytes_b.iter()).position(|(a, b)| a != b);
                        anyhow::bail!(
                            "file {name} is not byte-identical \
                             ({} vs {} bytes, first diff at offset {:?})",
                            bytes_a.len(),
                            bytes_b.len(),
                            first_diff,
                        );
                    }

                    log::info!("  {name}: OK ({} bytes)", bytes_a.len());
                }

                log::info!("all artifacts match!");
                Ok(())
            }
        });

        ctx.emit_side_effect_step([step], [done]);

        Ok(())
    }
}
