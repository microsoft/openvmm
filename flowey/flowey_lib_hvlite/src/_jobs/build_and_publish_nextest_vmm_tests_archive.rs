// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and publish a nextest archive containing VMM tests.

use crate::build_nextest_vmm_tests::BuildNextestVmmTestsMode;
use crate::run_cargo_build::common::CommonProfile;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        /// Build and run unit tests for the specified target
        pub target: target_lexicon::Triple,
        /// Build and run unit tests with the specified cargo profile
        pub profile: CommonProfile,

        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_nextest_vmm_tests_archive::publish::Node>();
        ctx.import::<crate::build_nextest_vmm_tests::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            profile,
            artifact_dir,
            done,
        } = request;

        let vmm_tests = ctx.reqv(|v| crate::build_nextest_vmm_tests::Request {
            profile,
            target,
            build_mode: BuildNextestVmmTestsMode::Archive(v),
        });

        ctx.req(
            crate::artifact_nextest_vmm_tests_archive::publish::Request {
                vmm_tests,
                artifact_dir,
                done,
            },
        );

        Ok(())
    }
}
