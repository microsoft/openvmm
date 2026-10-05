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
//! This node supports Windows and running Windows binaries through WSL2.
//!
//! See also: stop_test_igvm_agent_rpc_server for cleanup after tests complete.

use crate::build_test_igvm_agent_rpc_server::TestIgvmAgentRpcServerOutput;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// IGVM agent binary
        pub test_igvm_agent_rpc_server: ReadVar<TestIgvmAgentRpcServerOutput>,
        /// Environment variables from init_vmm_tests_env (contains VMM_TESTS_CONTENT_DIR and TEST_OUTPUT_PATH)
        pub env: ReadVar<BTreeMap<String, String>>,
        /// Completion indicator - signals that the server is ready
        pub done: WriteVar<SideEffect>,
        /// Used to ensure that the previous test run is complete, if any
        pub previous_done: Option<ReadVar<SideEffect>>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            test_igvm_agent_rpc_server,
            env,
            done,
            previous_done,
        } = request;

        ctx.emit_rust_step("starting test_igvm_agent_rpc_server", |ctx| {
            let test_igvm_agent_rpc_server = test_igvm_agent_rpc_server.claim(ctx);
            let env = env.claim(ctx);
            done.claim(ctx);
            previous_done.claim(ctx);
            move |rt| start_rpc_server(rt, test_igvm_agent_rpc_server, env)
        });

        Ok(())
    }
}

fn start_rpc_server(
    rt: &mut RustRuntimeServices<'_>,
    test_igvm_agent_rpc_server: ReadVar<TestIgvmAgentRpcServerOutput, VarClaimed>,
    env: ReadVar<BTreeMap<String, String>, VarClaimed>,
) -> anyhow::Result<()> {
    use std::path::Path;

    let windows_via_wsl2 = flowey_lib_common::_util::running_in_wsl(rt);
    if !matches!(rt.platform(), FlowPlatform::Windows) && !windows_via_wsl2 {
        anyhow::bail!("run_test_igvm_agent_rpc_server only supports Windows or WSL2");
    }

    let env = rt.read(env);

    let test_output_path = env
        .get("TEST_OUTPUT_PATH")
        .context("TEST_OUTPUT_PATH not set")?;

    let TestIgvmAgentRpcServerOutput { exe, .. } = rt.read(test_igvm_agent_rpc_server);

    // init_vmm_tests_env supplies Windows paths when targeting Windows from WSL2.
    let test_output_path = if windows_via_wsl2 {
        flowey_lib_common::_util::wslpath::win_to_linux(rt, test_output_path)?
    } else {
        Path::new(test_output_path).to_path_buf()
    };
    let log_file_path = test_output_path.join("test_igvm_agent_rpc_server.log");
    let log_file = std::fs::File::create(&log_file_path)?;
    let log_file_stderr = log_file.try_clone()?;

    log::info!(
        "starting test_igvm_agent_rpc_server from {}, logs at: {}",
        exe.display(),
        log_file_path.display()
    );

    let mut command = std::process::Command::new(&exe);
    command
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(log_file_stderr);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        // Use CREATE_NEW_PROCESS_GROUP so it doesn't receive console signals.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to spawn test_igvm_agent_rpc_server: {}",
            exe.display()
        )
    })?;

    // Give the server a moment to start up and bind to the RPC endpoint.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Check if the server is still running
    match child.try_wait()? {
        Some(status) => {
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
