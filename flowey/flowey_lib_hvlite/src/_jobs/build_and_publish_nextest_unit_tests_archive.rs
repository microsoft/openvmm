// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and publish a nextest archive containing HvLite repo unit tests.

use crate::build_nextest_unit_tests::BuildNextestUnitTestMode;
use crate::run_cargo_build::common::CommonProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_nextest_run::build_params::PanicAbortTests;

flowey_request! {
    pub struct Params {
        /// Build and run unit tests for the specified target
        pub target: target_lexicon::Triple,
        /// Build and run unit tests with the specified cargo profile
        pub profile: CommonProfile,
        /// Whether to build tests with unstable `-Zpanic-abort-tests` flag
        pub unstable_panic_abort_tests: Option<PanicAbortTests>,

        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_nextest_unit_tests_archive::publish::Node>();
        ctx.import::<crate::build_nextest_unit_tests::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            profile,
            unstable_panic_abort_tests,
            artifact_dir,
            done,
        } = request;

        let unit_tests = ctx.reqv(|v| crate::build_nextest_unit_tests::Request {
            profile,
            target,
            unstable_panic_abort_tests,
            build_mode: BuildNextestUnitTestMode::Archive(v),
        });

        ctx.req(
            crate::artifact_nextest_unit_tests_archive::publish::Request {
                unit_tests,
                artifact_dir,
                done,
            },
        );

        Ok(())
    }
}
