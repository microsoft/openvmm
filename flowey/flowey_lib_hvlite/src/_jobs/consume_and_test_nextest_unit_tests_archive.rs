// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run a pre-built cargo-nextest based unit test archive.

use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Params {
        /// Friendly label for report JUnit test results
        pub junit_test_label: String,
        /// Existing unit test archive artifact dir
        pub unit_test_artifact_dir: ReadVar<PathBuf>,
        /// Nextest profile to use when running the source code
        pub nextest_profile: NextestProfile,

        /// Whether the job should fail if any test has failed
        pub fail_job_on_test_fail: bool,
        /// (optionally) Also publish raw junit.xml test results as an artifact.
        pub artifact_dir: Option<ReadVar<PathBuf>>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
        ctx.import::<crate::artifact_nextest_unit_tests_archive::resolve::Node>();
        ctx.import::<crate::test_nextest_unit_tests_archive::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            junit_test_label,
            unit_test_artifact_dir,
            nextest_profile,
            fail_job_on_test_fail,
            artifact_dir,
            done,
        } = request;

        let nextest_archive_file =
            ctx.reqv(
                |v| crate::artifact_nextest_unit_tests_archive::resolve::Request {
                    artifact_dir: unit_test_artifact_dir,
                    nextest_archive: v,
                },
            );

        let results = ctx.reqv(|v| crate::test_nextest_unit_tests_archive::Request {
            nextest_archive_file,
            nextest_profile,
            results: v,
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
