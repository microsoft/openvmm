// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and run the cargo-nextest based unit tests.

use crate::build_nextest_unit_tests::BuildNextestUnitTestMode;
use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

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
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
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

        let results = ctx.reqv(|v| crate::build_nextest_unit_tests::Request {
            profile,
            target,
            build_mode: BuildNextestUnitTestMode::ImmediatelyRun {
                nextest_profile,
                results: v,
            },
        });

        let mut side_effects = Vec::new();

        let junit_xml = results.map(ctx, |r| r.junit_xml);
        let reported_results = ctx.reqv(|v| flowey_lib_common::publish_test_results::Request {
            junit_xml,
            test_label: junit_test_label,
            attachments: BTreeMap::new(),
            output_dir: artifact_dir,
            done: v,
        });

        side_effects.push(reported_results);

        ctx.emit_rust_step("report test results to overall pipeline status", |ctx| {
            side_effects.claim(ctx);
            done.claim(ctx);

            let results = results.clone().claim(ctx);
            move |rt| {
                let results = rt.read(results);
                if results.all_tests_passed {
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
