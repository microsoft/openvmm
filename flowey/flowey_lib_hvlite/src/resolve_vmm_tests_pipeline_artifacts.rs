// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolve artifacts from the test content dir used as parts of the pipeline
//! to run VMM tests.

use crate::init_vmm_tests_content_dir::VmmTestsBuiltArtifactsWrite;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// Directory to symlink / copy test contents into. Does not need to be
        /// empty.
        pub test_content_dir: ReadVar<PathBuf>,
        /// Artifacts to resolve
        pub built_artifacts_write: VmmTestsBuiltArtifactsWrite,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            test_content_dir,
            built_artifacts_write,
        } = request;

        ctx.emit_rust_step("resolving vmm tests pipeline artifacts", |ctx| {
            claim_vars!(ctx, (test_content_dir, built_artifacts_write));

            move |rt| {
                let test_content_dir = rt.read(test_content_dir);

                built_artifacts_write.resolve(rt, test_content_dir)?;

                Ok(())
            }
        });

        Ok(())
    }
}
