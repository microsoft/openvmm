// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helper to streamline copying an ad-hoc set of files into an artifact
//! directory.

use crate::_util::copy_dir_all;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// Friendly label printed when running the step.
        pub debug_label: String,
        /// Path to the artifact directory
        pub artifact_dir: ReadVar<PathBuf>,
        /// A collection of (dst, src) pairs of files to copy into the artifact dir.
        pub files: ReadVar<Vec<(PathBuf, PathBuf)>>,
        /// Signal that the file copy succeeded.
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            debug_label,
            files,
            artifact_dir,
            done,
        } = request;

        ctx.emit_rust_step(format!("copying {debug_label} to artifact dir"), |ctx| {
            done.claim(ctx);
            let files = files.claim(ctx);
            let artifact_dir = artifact_dir.claim(ctx);
            move |rt| {
                let artifact_dir = rt.read(artifact_dir);
                let files = rt.read(files);

                for (dst, src) in files {
                    // allow passing things like `some/subdir/artifact`
                    if let Some(parent) = dst.parent() {
                        fs_err::create_dir_all(artifact_dir.join(parent))?;
                    }

                    let dst = artifact_dir.join(dst);
                    if src.is_dir() {
                        copy_dir_all(src, dst)?;
                    } else {
                        fs_err::copy(src, dst)?;
                    }
                }

                log::info!("copied files into {}", artifact_dir.display());

                Ok(())
            }
        });

        Ok(())
    }
}
