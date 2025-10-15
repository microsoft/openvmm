// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run cargo-nextest list.
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// Path to nextest archive file
        pub archive_file: ReadVar<PathBuf>,
        /// Path to nextest binary
        pub nextest_bin: Option<ReadVar<PathBuf>>,
        /// Target triple for the build
        pub target: Option<ReadVar<target_lexicon::Triple>>,
        /// Working directory the test archive was created from.
        pub working_dir: Option<ReadVar<PathBuf>>,
        /// Path to `.config/nextest.toml`
        pub config_file: Option<ReadVar<PathBuf>>,
        /// Nextest profile to use when running the source code
        pub nextest_profile: String,
        /// Nextest test filter expression
        pub nextest_filter_expr: Option<String>,
        /// Whether to include ignored tests in the list output
        pub run_ignored: bool,
        /// Additional env vars set when executing the tests.
        pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
        /// Output directory for the nextest list output file
        pub output_dir: ReadVar<PathBuf>,
        /// Wait for specified side-effects to resolve
        pub pre_run_deps: Vec<ReadVar<SideEffect>>,
        /// Final path to nextest list output file
        pub output_file: WriteVar<PathBuf>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::run_cargo_nextest_list::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let openvmm_repo_path = crate::run_cargo_nextest_run::get_openvmm_repo_path(ctx);

        let default_nextest_config_file = crate::run_cargo_nextest_run::default_nextest_config_file(
            openvmm_repo_path.clone(),
            ctx,
        );

        let base_env = crate::run_cargo_nextest_run::base_env();

        let Request {
            archive_file,
            nextest_bin,
            target,
            working_dir,
            config_file,
            nextest_profile,
            nextest_filter_expr,
            run_ignored,
            extra_env,
            output_dir,
            mut pre_run_deps,
            output_file,
        } = request;

        let extra_env =
            crate::run_cargo_nextest_run::merged_extra_env(extra_env, base_env.clone(), ctx);

        let working_dir = crate::run_cargo_nextest_run::resolve_working_dir(
            working_dir,
            openvmm_repo_path.clone(),
            &mut pre_run_deps,
        );

        let config_file = crate::run_cargo_nextest_run::resolve_config_file(
            config_file,
            default_nextest_config_file.clone(),
            &mut pre_run_deps,
        );

        ctx.req(flowey_lib_common::run_cargo_nextest_list::Request {
            archive_file,
            nextest_bin,
            target,
            working_dir,
            config_file,
            nextest_profile,
            nextest_filter_expr,
            run_ignored,
            extra_env: Some(extra_env),
            output_dir,
            pre_run_deps,
            output_file,
        });

        Ok(())
    }
}
