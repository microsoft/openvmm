// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run cargo-nextest based unit tests from a pre-built archive.
//!
//! In the context of hvlite, we consider a "unit-test" to be any test which
//! doesn't require any special dependencies (e.g: additional binaries, disk
//! images, etc...), and can be run simply by invoking the test bin itself.

use crate::build_nextest_unit_tests::NextestUnitTestArchive;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_nextest_run::TestResults;

flowey_request! {
    pub struct Request {
        /// Pre-built unit tests nextest archive
        pub nextest_archive_file: ReadVar<NextestUnitTestArchive>,
        /// Nextest profile to use when running the source code
        pub nextest_profile: NextestProfile,
        /// Optionally provide the nextest bin to use
        pub nextest_bin: Option<ReadVar<PathBuf>>,
        /// Target for the tests to run on
        pub target: Option<ReadVar<target_lexicon::Triple>>,
        /// Results of running the tests
        pub results: WriteVar<TestResults>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_nextest_run::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        for Request {
            nextest_archive_file,
            nextest_profile,
            nextest_bin,
            target,
            results,
        } in requests
        {
            let nextest_archive = nextest_archive_file.map(ctx, |x| x.archive_file);

            ctx.req(crate::run_cargo_nextest_run::Request {
                friendly_name: "unit-tests".into(),
                run_kind:
                    flowey_lib_common::run_cargo_nextest_run::NextestRunKind::RunFromArchive {
                        archive_file: nextest_archive,
                        target,
                        nextest_bin,
                    },
                nextest_profile,
                nextest_filter_expr: None,
                nextest_working_dir: None,
                nextest_config_file: None,
                nextest_partition: None,
                run_ignored: false,
                extra_env: None,
                pre_run_deps: Vec::new(), // FIXME: ensure all deps are installed
                results,
            })
        }

        Ok(())
    }
}
