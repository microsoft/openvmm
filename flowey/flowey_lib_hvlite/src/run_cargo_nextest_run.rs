// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run cargo-nextest tests in the context of the HvLite repo.
//!
//! Uses the generic [`flowey_lib_common::run_cargo_nextest_run::Node`]
//! under-the-hood.

use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_nextest_run::NextestRunKind;
use flowey_lib_common::run_cargo_nextest_run::TestResults;
use std::collections::BTreeMap;

/// Nextest profiles defined in HvLite's `.config/nextest.toml`
#[derive(Serialize, Deserialize)]
pub enum NextestProfile {
    Default,
    Ci,
}

impl NextestProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            NextestProfile::Default => "default",
            NextestProfile::Ci => "ci",
        }
    }
}

flowey_request! {
    pub struct Request {
        /// Friendly name for this test group that will be displayed in logs.
        pub friendly_name: String,
        /// What kind of test run this is (inline build vs. from nextest archive).
        pub run_kind: NextestRunKind,
        /// Nextest profile to use when running the source code
        pub nextest_profile: NextestProfile,
        /// Nextest test filter expression
        pub nextest_filter_expr: Option<String>,
        /// Nextest working directory (defaults to repo root)
        pub nextest_working_dir: Option<ReadVar<PathBuf>>,
        /// Nextest configuration file (defaults to config in repo)
        pub nextest_config_file: Option<ReadVar<PathBuf>>,
        /// Whether to run ignored test
        pub run_ignored: bool,
        /// Additional env vars set when executing the tests.
        pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
        /// Wait for specified side-effects to resolve before building / running any
        /// tests. (e.g: to allow for some ambient packages / dependencies to
        /// get installed).
        pub pre_run_deps: Vec<ReadVar<SideEffect>>,
        /// Results of running the tests
        pub results: WriteVar<TestResults>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::run_cargo_nextest_run::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let default_nextest_config_file =
            openvmm_repo_path.map(ctx, |p| p.join(".config").join("nextest.toml"));

        let base_env = [
            // Used by the test_with_tracing macro in test runners
            ("RUST_LOG", "trace"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect::<BTreeMap<_, _>>();

        for Request {
            friendly_name,
            run_kind,
            nextest_profile,
            nextest_filter_expr,
            nextest_working_dir,
            nextest_config_file,
            run_ignored,
            mut pre_run_deps,
            results,
            extra_env,
        } in requests
        {
            let extra_env = if let Some(with_env) = extra_env {
                let base_env = base_env.clone();
                with_env.map(ctx, move |mut m| {
                    m.extend(base_env);
                    m
                })
            } else {
                ReadVar::from_static(base_env.clone())
            };

            let working_dir = if let Some(nextest_working_dir) = nextest_working_dir {
                pre_run_deps.push(openvmm_repo_path.clone().into_side_effect());
                nextest_working_dir
            } else {
                openvmm_repo_path.clone()
            };

            let config_file = if let Some(nextest_config_file) = nextest_config_file {
                pre_run_deps.push(default_nextest_config_file.clone().into_side_effect());
                nextest_config_file
            } else {
                default_nextest_config_file.clone()
            };

            ctx.req(flowey_lib_common::run_cargo_nextest_run::Request::Run(
                flowey_lib_common::run_cargo_nextest_run::Run {
                    friendly_name,
                    run_kind,
                    working_dir,
                    config_file,
                    tool_config_files: Vec::new(),
                    nextest_profile: nextest_profile.as_str().to_owned(),
                    extra_env: Some(extra_env),
                    with_rlimit_unlimited_core_size: true,
                    nextest_filter_expr,
                    run_ignored,
                    pre_run_deps,
                    results,
                },
            ));
        }

        Ok(())
    }
}
