// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run cargo-nextest list subcommand.
use crate::gen_cargo_nextest_run_cmd;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// What kind of test run this is (inline build vs. from nextest archive).
        pub run_kind_deps: gen_cargo_nextest_run_cmd::RunKindDeps,
        /// Working directory the test archive was created from.
        pub working_dir: ReadVar<PathBuf>,
        /// Path to `.config/nextest.toml`
        pub config_file: ReadVar<PathBuf>,
        /// Nextest profile to use when running the source code
        pub nextest_profile: String,
        /// Nextest test filter expression
        pub nextest_filter_expr: Option<String>,
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
            run_kind_deps,
            working_dir,
            config_file,
            nextest_profile,
            nextest_filter_expr,
            extra_env,
            command: list_cmd,
        } in requests
        {
            if let gen_cargo_nextest_run_cmd::RunKindDeps::BuildAndRun {
                params: _,
                nextest_installed: _,
                rust_toolchain: _,
                cargo_flags: _,
            } = run_kind_deps
            {
                anyhow::bail!("BuildAndRun is not supported.")
            }

            let run_cmd = ctx.reqv(|v| gen_cargo_nextest_run_cmd::Request {
                run_kind_deps,
                working_dir,
                config_file,
                tool_config_files: Vec::new(), // Ignored
                nextest_profile,
                extra_env,
                nextest_filter_expr,
                run_ignored: true,
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
