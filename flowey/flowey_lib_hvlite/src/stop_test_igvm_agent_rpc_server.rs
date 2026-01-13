// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stop the test_igvm_agent_rpc_server after VMM tests complete.
//!
//! This node terminates any running test_igvm_agent_rpc_server.exe processes
//! using taskkill. It should be run after VMM tests complete to clean up
//! the background server process started by run_test_igvm_agent_rpc_server.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// Dependency to ensure this runs after tests complete
        pub after_tests: ReadVar<SideEffect>,
        /// Completion indicator - signals that the server has been stopped
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { after_tests, done } = request;

        ctx.emit_rust_step("stopping test_igvm_agent_rpc_server", |ctx| {
            after_tests.claim(ctx);
            done.claim(ctx);
            move |_rt| {
                #[cfg(not(windows))]
                {
                    log::info!("test_igvm_agent_rpc_server is Windows-only, nothing to stop");
                    Ok(())
                }

                #[cfg(windows)]
                {
                    log::info!("stopping test_igvm_agent_rpc_server processes");

                    // Use taskkill to terminate any running instances
                    let output = std::process::Command::new("taskkill")
                        .args(["/F", "/IM", "test_igvm_agent_rpc_server.exe"])
                        .output();

                    match output {
                        Ok(output) => {
                            let stdout = String::from_utf8_lossy(&output.stdout);
                            let stderr = String::from_utf8_lossy(&output.stderr);

                            if output.status.success() {
                                log::info!(
                                    "test_igvm_agent_rpc_server terminated: {}",
                                    stdout.trim()
                                );
                            } else if stderr.contains("not found") || stdout.contains("not found") {
                                // Process wasn't running - that's fine
                                log::info!("test_igvm_agent_rpc_server was not running");
                            } else {
                                log::warn!(
                                    "taskkill returned non-zero: stdout={}, stderr={}",
                                    stdout.trim(),
                                    stderr.trim()
                                );
                            }
                        }
                        Err(e) => {
                            log::warn!("failed to run taskkill: {}", e);
                        }
                    }

                    Ok(())
                }
            }
        });

        Ok(())
    }
}
