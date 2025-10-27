// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run cargo-nextest list command.
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::io::Write;
use std::process::ExitStatus;
use std::process::Stdio;

flowey_request! {
    pub struct Request {
        /// Path to nextest archive file
        pub archive_file: ReadVar<PathBuf>,
        /// Path to nextest binary
        pub nextest_bin: Option<ReadVar<PathBuf>>,
        /// Target triple for the build
        pub target: Option<ReadVar<target_lexicon::Triple>>,
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
        ctx.import::<crate::download_cargo_nextest::Node>();
        ctx.import::<crate::gen_cargo_nextest_list_cmd::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
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
            pre_run_deps,
            output_file,
        } = request;

        let target = target.unwrap_or(ReadVar::from_static(target_lexicon::Triple::host()));

        let nextest_bin = nextest_bin.unwrap_or_else(|| {
            ctx.reqv(|v| crate::download_cargo_nextest::Request::Get(target.clone(), v))
        });

        let cmd = ctx.reqv(|v| crate::gen_cargo_nextest_list_cmd::Request {
            archive_file,
            nextest_bin,
            target,
            working_dir: working_dir.clone(),
            config_file,
            nextest_profile,
            nextest_filter_expr,
            run_ignored,
            extra_env,
            command: v,
        });

        ctx.emit_rust_step("run cargo-nextest list", |ctx| {
            pre_run_deps.claim(ctx);
            let cmd = cmd.claim(ctx);
            let working_dir = working_dir.claim(ctx);
            let output_file = output_file.claim(ctx);
            let output_dir = output_dir.claim(ctx);

            move |rt| {
                let working_dir = rt.read(working_dir);
                let cmd = rt.read(cmd);
                let output_dir = rt.read(output_dir);

                let (status, stdout) = run_command(&cmd, &working_dir, true)?;

                if !status.success() {
                    anyhow::bail!(
                        "cargo-nextest list command failed with exit code: {}",
                        status.code().unwrap_or(-1)
                    );
                }

                let stdout = stdout
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("failed to capture stdout"))?;

                let json_value = get_nextest_list_output_from_stdout(stdout)?;

                let final_path = output_dir.join("nextest_list_output.json");
                let mut file = fs_err::File::create(&final_path)?;
                file.write_all(serde_json::to_string_pretty(&json_value)?.as_bytes())?;
                file.flush()?;

                rt.write(output_file, &final_path.to_path_buf());

                Ok(())
            }
        });

        Ok(())
    }
}

fn run_command(
    cmd: &crate::gen_cargo_nextest_run_cmd::Script,
    working_dir: &PathBuf,
    capture_stdout: bool,
) -> anyhow::Result<(ExitStatus, Option<String>)> {
    let mut command = std::process::Command::new(&cmd.commands[0].0);
    command
        .args(&cmd.commands[0].1)
        .envs(&cmd.env)
        .current_dir(working_dir);

    if capture_stdout {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::inherit());
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn '{}'", &cmd.commands[0].0.to_string_lossy()))?;

    if capture_stdout {
        let output = child.wait_with_output()?;
        let stdout_str = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok((output.status, Some(stdout_str)))
    } else {
        let status = child.wait()?;
        Ok((status, None))
    }
}

fn get_nextest_list_output_from_stdout(output: &str) -> anyhow::Result<serde_json::Value> {
    // nextest list prints a few lines of non-json output before the actual
    // JSON output, so we need to find the first line that is valid JSON
    for line in output.lines() {
        if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(line) {
            return Ok(json_value);
        }
    }
    anyhow::bail!("failed to find JSON output in nextest list command output");
}
