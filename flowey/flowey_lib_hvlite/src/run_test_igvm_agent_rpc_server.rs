// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Start the test_igvm_agent_rpc_server before running VMM tests.
//!
//! The RPC server provides a fake IGVM agent attestation endpoint for
//! CVM TPM guest tests. It must be running before the tests start and
//! stay alive for the duration of the test run.
//!
//! This node starts the server from the test content directory (where
//! init_vmm_tests_env copies the binary) and redirects output to a log file.
//!
//! See also: stop_test_igvm_agent_rpc_server for cleanup after tests complete.

use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// Environment variables from init_vmm_tests_env (contains VMM_TESTS_CONTENT_DIR and TEST_OUTPUT_PATH)
        pub env: ReadVar<BTreeMap<String, String>>,
        /// Completion indicator - signals that the server is ready
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { env, done } = request;

        ctx.emit_rust_step("starting test_igvm_agent_rpc_server", |ctx| {
            let env = env.claim(ctx);
            done.claim(ctx);
            move |rt| {
                let env = rt.read(env);

                // Only run on Windows - the RPC server is Windows-only
                #[cfg(not(windows))]
                {
                    let _ = env;
                    log::info!("test_igvm_agent_rpc_server is Windows-only, skipping");
                    Ok(())
                }

                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;

                    // Get paths from environment
                    let test_content_dir = env
                        .get("VMM_TESTS_CONTENT_DIR")
                        .context("VMM_TESTS_CONTENT_DIR not set")?;
                    let test_output_path = env
                        .get("TEST_OUTPUT_PATH")
                        .context("TEST_OUTPUT_PATH not set")?;

                    let exe = Path::new(test_content_dir).join("test_igvm_agent_rpc_server.exe");

                    if !exe.exists() {
                        log::info!(
                            "test_igvm_agent_rpc_server.exe not found at {}, skipping",
                            exe.display()
                        );
                        return Ok(());
                    }

                    // Create log file for server output
                    let log_file_path =
                        Path::new(test_output_path).join("test_igvm_agent_rpc_server.log");
                    let log_file = std::fs::File::create(&log_file_path)?;
                    let log_file_stderr = log_file.try_clone()?;

                    log::info!(
                        "starting test_igvm_agent_rpc_server from {}, logs at: {}",
                        exe.display(),
                        log_file_path.display()
                    );

                    // Spawn the RPC server as a background process.
                    // Use CREATE_NEW_PROCESS_GROUP so it doesn't receive console signals.
                    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

                    let mut child = std::process::Command::new(&exe)
                        .stdin(std::process::Stdio::null())
                        .stdout(log_file)
                        .stderr(log_file_stderr)
                        .creation_flags(CREATE_NEW_PROCESS_GROUP)
                        .spawn()
                        .with_context(|| {
                            format!(
                                "failed to spawn test_igvm_agent_rpc_server: {}",
                                exe.display()
                            )
                        })?;

                    // Give the server a moment to start up and bind to the RPC endpoint.
                    // The server closes stdout when it's ready, but since we redirected
                    // stdout to a file, we can't detect that. Instead, we poll briefly.
                    std::thread::sleep(std::time::Duration::from_millis(500));

                    // Check if the server is still running
                    match child.try_wait()? {
                        Some(status) => {
                            // Server exited - this is an error unless endpoint was already in use
                            anyhow::bail!(
                                "test_igvm_agent_rpc_server exited unexpectedly with status: {:?}. \
                                Check logs at: {}",
                                status.code(),
                                log_file_path.display()
                            );
                        }
                        None => {
                            log::info!(
                                "test_igvm_agent_rpc_server started successfully (pid: {})",
                                child.id()
                            );
                        }
                    }

                    // Don't wait on the child - let it run in the background.
                    // The process will be cleaned up by stop_test_igvm_agent_rpc_server
                    // after tests complete. We intentionally drop the Child handle.
                    drop(child);

                    Ok(())
                }
            }
        });

        Ok(())
    }
}
