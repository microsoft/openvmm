// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Job to build vmm_tests and discover required artifacts.

use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use std::path::PathBuf;

flowey_request! {
    pub struct Params {
        /// Target triple for cross-compilation
        pub target: CommonTriple,
        /// Test filter to use when discovering artifacts (nextest filter expression)
        pub filter: String,
        /// Output file for the discovered artifacts JSON.
        /// If not specified, outputs to stdout.
        pub output: Option<PathBuf>,
        /// Release build instead of debug build
        pub release: bool,
        /// Handle to signal job completion
        pub done: WriteVar<SideEffect>,
        /// If set, also write the artifacts JSON string to this variable.
        pub artifacts_json_out: Option<WriteVar<String>>,
        /// Additional side-effect dependencies to wait for before building
        /// (e.g., install_cargo_nextest in CI).
        pub pre_build_done: Vec<ReadVar<SideEffect>>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::install_openvmm_rust_build_essential::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            filter,
            output,
            release,
            done,
            artifacts_json_out,
            pre_build_done,
        } = request;

        let target_str = target.as_triple().to_string();
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let build_essential = ctx.reqv(crate::install_openvmm_rust_build_essential::Request);

        ctx.emit_rust_step("build vmm_tests and discover artifacts", |ctx| {
            done.claim(ctx);
            build_essential.claim(ctx);
            for dep in pre_build_done {
                dep.claim(ctx);
            }
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            let artifacts_json_out = artifacts_json_out.map(|v| v.claim(ctx));
            move |rt| {
                let openvmm_repo_path = rt.read(openvmm_repo_path);

                log::info!(
                    "Discovering artifacts for filter: {} (target: {})",
                    filter,
                    target_str
                );

                // Step 1: Use nextest to resolve the filter expression to test names and get binary path
                rt.sh.change_dir(&openvmm_repo_path);
                let mut cmd = flowey::shell_cmd!(
                    rt,
                    "cargo nextest list -p vmm_tests --target {target_str} --filter-expr {filter} --message-format json"
                );
                if release {
                    cmd = cmd.arg("--release");
                }
                let nextest_output = cmd.output()?;
                anyhow::ensure!(
                    nextest_output.status.success(),
                    "cargo nextest list failed: {}",
                    String::from_utf8_lossy(&nextest_output.stderr)
                );
                let nextest_stdout = String::from_utf8(nextest_output.stdout)
                    .map_err(|e| anyhow::anyhow!("nextest output is not valid UTF-8: {}", e))?;
                let (test_binary, test_names) = parse_nextest_output(&nextest_stdout)?;

                if test_names.is_empty() {
                    log::warn!("No tests match the filter: {}", filter);
                    // Output empty artifact list with target info
                    let empty_output = serde_json::json!({
                        "target": target_str,
                        "required": [],
                        "optional": []
                    });
                    let empty_output_str = serde_json::to_string_pretty(&empty_output)?;
                    if let Some(output_path) = output {
                        std::fs::write(&output_path, &empty_output_str)?;
                        log::info!("Wrote empty artifact list to: {}", output_path.display());
                    } else {
                        println!("{}", empty_output_str);
                    }
                    if let Some(var) = artifacts_json_out {
                        rt.write(var, &empty_output_str);
                    }
                    return Ok(());
                }

                log::info!("Found {} matching tests", test_names.len());
                for name in &test_names {
                    log::debug!("  - {}", name);
                }

                // Step 2: Query petri for artifacts of each matching test
                log::info!("Using test binary: {}", test_binary.display());
                log::info!(
                    "Querying artifacts for {} tests in a single invocation",
                    test_names.len()
                );
                let stdin_data = test_names.iter().map(|n| format!("{n}\n")).collect::<String>();
                let artifact_output = flowey::shell_cmd!(
                    rt,
                    "{test_binary} --list-required-artifacts --tests-from-stdin"
                )
                .stdin(stdin_data)
                .output()?;
                anyhow::ensure!(
                    artifact_output.status.success(),
                    "test binary failed: {}",
                    String::from_utf8_lossy(&artifact_output.stderr)
                );
                let artifact_stdout = String::from_utf8(artifact_output.stdout)
                    .map_err(|e| anyhow::anyhow!("test output is not valid UTF-8: {}", e))?;
                let json_output = parse_artifacts_output(&artifact_stdout, &target_str)?;

                if let Some(output_path) = output {
                    std::fs::write(&output_path, &json_output)
                        .map_err(|e| anyhow::anyhow!("failed to write output file: {}", e))?;
                    log::info!("Wrote artifact list to: {}", output_path.display());
                } else {
                    // Output to stdout
                    println!("{}", json_output);
                }

                if let Some(var) = artifacts_json_out {
                    rt.write(var, &json_output);
                }

                Ok(())
            }
        });

        Ok(())
    }
}

/// Parse `cargo nextest list --message-format json` output to extract test names and binary path.
fn parse_nextest_output(stdout: &str) -> anyhow::Result<(PathBuf, Vec<String>)> {
    let json: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| anyhow::anyhow!("failed to parse nextest JSON output: {}", e))?;

    let mut test_names = Vec::new();
    let mut binary_path = None;

    // Navigate to rust-suites -> vmm_tests::tests -> testcases
    if let Some(vmm_tests) = json
        .get("rust-suites")
        .and_then(|s| s.get("vmm_tests::tests"))
    {
        // Get the binary path
        if let Some(path) = vmm_tests.get("binary-path").and_then(|v| v.as_str()) {
            binary_path = Some(PathBuf::from(path));
        }

        if let Some(testcases_obj) = vmm_tests.get("testcases").and_then(|t| t.as_object()) {
            for (test_name, test_info) in testcases_obj {
                // Check if filter-match.status == "matches"
                let matches = test_info
                    .get("filter-match")
                    .and_then(|fm| fm.get("status"))
                    .and_then(|s| s.as_str())
                    == Some("matches");

                if matches {
                    test_names.push(test_name.clone());
                }
            }
        }
    }

    let binary_path = binary_path
        .ok_or_else(|| anyhow::anyhow!("Could not find test binary path in nextest output"))?;

    Ok((binary_path, test_names))
}

/// Parse test binary `--list-required-artifacts` JSON output and add target info.
fn parse_artifacts_output(stdout: &str, target: &str) -> anyhow::Result<String> {
    let json: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|e| anyhow::anyhow!("failed to parse test output JSON: {}", e))?;

    let required = json
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let optional = json
        .get("optional")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Build the combined JSON output with target info
    let output = serde_json::json!({
        "target": target,
        "required": required,
        "optional": optional,
    });

    Ok(serde_json::to_string_pretty(&output)?)
}
