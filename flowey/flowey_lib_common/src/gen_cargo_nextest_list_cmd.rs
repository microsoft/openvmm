// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run cargo-nextest list subcommand.
use crate::gen_cargo_nextest_run_cmd;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// Path to nextest archive file
        pub archive_file: ReadVar<PathBuf>,
        /// Path to nextest binary
        pub nextest_bin: ReadVar<PathBuf>,
        /// Target triple for the build
        pub target: ReadVar<target_lexicon::Triple>,
        /// Working directory the test archive was created from.
        pub working_dir: ReadVar<PathBuf>,
        /// Path to `.config/nextest.toml`
        pub config_file: ReadVar<PathBuf>,
        /// Nextest profile to use when running the source code
        pub nextest_profile: String,
        /// Nextest test filter expression
        pub nextest_filter_expr: Option<String>,
        /// Whether to include ignored tests in the list output
        pub run_ignored: bool,
        /// Additional env vars set when executing the tests.
        pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
        /// Generated cargo-nextest list command
        pub command: WriteVar<gen_cargo_nextest_run_cmd::Command>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<gen_cargo_nextest_run_cmd::Node>();
    }

    fn emit(requests: Vec<Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        for Request {
            archive_file,
            nextest_bin,
            target,
            working_dir,
            config_file,
            nextest_profile,
            nextest_filter_expr,
            run_ignored,
            extra_env,
            command: list_cmd,
        } in requests
        {
            let run_cmd = ctx.reqv(|v| gen_cargo_nextest_run_cmd::Request {
                run_kind_deps: gen_cargo_nextest_run_cmd::RunKindDeps::RunFromArchive {
                    archive_file,
                    nextest_bin,
                    target,
                },
                working_dir,
                config_file,
                tool_config_files: Vec::new(), // Ignored
                nextest_profile,
                extra_env,
                nextest_filter_expr,
                run_ignored,
                fail_fast: None,
                portable: false,
                command: v,
            });

            ctx.emit_rust_step("generate nextest list command", |ctx| {
                let run_cmd = run_cmd.claim(ctx);
                let list_cmd = list_cmd.claim(ctx);
                move |rt| {
                    let mut cmd = rt.read(run_cmd);
                    cmd.args = cmd
                        .args
                        .into_iter()
                        .map(|arg| if arg == "run" { "list".into() } else { arg })
                        .collect();
                    cmd.args.extend(["--message-format".into(), "json".into()]);

                    rt.write(list_cmd, &cmd);
                    log::info!("Generated command: {}", cmd);

                    Ok(())
                }
            });
        }
        Ok(())
    }
}
