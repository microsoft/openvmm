// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Job to build vmm_tests and discover required artifacts.

use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use std::path::Path;
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
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            filter,
            output,
            release,
            done,
        } = request;

        let target_str = target.as_triple().to_string();
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        ctx.emit_rust_step("build vmm_tests and discover artifacts", |ctx| {
            done.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            move |rt| {
                let openvmm_repo_path = rt.read(openvmm_repo_path);

                log::info!(
                    "Discovering artifacts for filter: {} (target: {})",
                    filter,
                    target_str
                );

                // Step 1: Use nextest to resolve the filter expression to test names and get binary path
                let (test_binary, test_names) = get_matching_tests_from_nextest(
                    &openvmm_repo_path,
                    &filter,
                    &target_str,
                    release,
                )?;

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
                    return Ok(());
                }

                log::info!("Found {} matching tests", test_names.len());
                for name in &test_names {
                    log::debug!("  - {}", name);
                }

                // Step 2: Query petri for artifacts of each matching test
                let json_output = get_artifacts_for_tests(
                    &openvmm_repo_path,
                    &test_binary,
                    &test_names,
                    &target_str,
                )?;

                if let Some(output_path) = output {
                    std::fs::write(&output_path, &json_output)
                        .map_err(|e| anyhow::anyhow!("failed to write output file: {}", e))?;
                    log::info!("Wrote artifact list to: {}", output_path.display());
                } else {
                    // Output to stdout
                    println!("{}", json_output);
                }

                Ok(())
            }
        });

        Ok(())
    }
}

/// Use `cargo nextest list` to resolve a filter expression to test names and get the binary path.
fn get_matching_tests_from_nextest(
    repo_path: &Path,
    filter: &str,
    target: &str,
    release: bool,
) -> anyhow::Result<(PathBuf, Vec<String>)> {
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("nextest")
        .arg("list")
        .arg("-p")
        .arg("vmm_tests")
        .arg("--target")
        .arg(target)
        .arg("--filter-expr")
        .arg(filter)
        .arg("--message-format")
        .arg("json");

    if release {
        cmd.arg("--release");
    }

    cmd.current_dir(repo_path);

    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run cargo nextest list: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("cargo nextest list failed: {}", stderr);
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| anyhow::anyhow!("nextest output is not valid UTF-8: {}", e))?;

    // Parse the JSON output to extract matching test names and binary path
    let json: serde_json::Value = serde_json::from_str(&stdout)
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

/// Query petri for artifacts of specific tests.
///
/// This function runs the test binary directly (bypassing cargo overhead)
/// to query artifacts for all tests at once using --tests-from-stdin.
fn get_artifacts_for_tests(
    repo_path: &Path,
    test_binary: &Path,
    test_names: &[String],
    target: &str,
) -> anyhow::Result<String> {
    use std::io::Write;

    log::info!("Using test binary: {}", test_binary.display());
    log::info!(
        "Querying artifacts for {} tests in a single invocation",
        test_names.len()
    );

    // Run the test binary with --tests-from-stdin to query all tests at once
    let mut child = std::process::Command::new(test_binary)
        .arg("--list-required-artifacts")
        .arg("--tests-from-stdin")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .current_dir(repo_path)
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn test binary: {}", e))?;

    // Write all test names to stdin
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("failed to open stdin"))?;
        for test_name in test_names {
            writeln!(stdin, "{}", test_name)
                .map_err(|e| anyhow::anyhow!("failed to write to stdin: {}", e))?;
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| anyhow::anyhow!("failed to wait for test binary: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("test binary failed: {}", stderr);
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| anyhow::anyhow!("test output is not valid UTF-8: {}", e))?;

    // Parse the JSON output and add target info
    let json: serde_json::Value = serde_json::from_str(&stdout)
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
