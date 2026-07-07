// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and run the cargo-nextest based unit tests.

use crate::build_nextest_unit_tests::BuildNextestUnitTestMode;
use crate::common::CommonProfile;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        /// Friendly label for report JUnit test results
        pub junit_test_label: String,
        /// Build and run unit tests for the specified target
        pub target: target_lexicon::Triple,
        /// Build and run unit tests with the specified cargo profile
        pub profile: CommonProfile,
        /// Nextest profile to use when running the source code
        pub nextest_profile: NextestProfile,

        /// Whether the job should fail if any test has failed
        pub fail_job_on_test_fail: bool,
        /// If provided, also publish junit.xml test results as an artifact.
        pub artifact_dir: Option<ReadVar<PathBuf>>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_nextest_unit_tests::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            junit_test_label,
            target,
            profile,
            nextest_profile,
            fail_job_on_test_fail,
            artifact_dir,
            done,
        } = request;

        let (publish_done, publish_done_write) = ctx.new_var();
        let results = ctx.reqv(|v| crate::build_nextest_unit_tests::Request {
            profile,
            target,
            build_mode: BuildNextestUnitTestMode::ImmediatelyRun {
                nextest_profile,
                junit_test_label,
                artifact_dir,
                results: v,
                publish_done: publish_done_write,
            },
        });

        ctx.emit_rust_step("report test results to overall pipeline status", |ctx| {
            publish_done.claim(ctx);
            done.claim(ctx);

            let results = results.clone().claim(ctx);
            move |rt| {
                let results = rt.read(results);
                if results.iter().all(|r| r.all_tests_passed) {
                    log::info!("all tests passed!");
                } else {
                    if fail_job_on_test_fail {
                        anyhow::bail!("encountered test failures.")
                    } else {
                        log::error!("encountered test failures.")
                    }
                }

                Ok(())
            }
        });

        Ok(())
    }
}
