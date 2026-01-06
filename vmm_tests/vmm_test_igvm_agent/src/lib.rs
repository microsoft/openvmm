// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for managing the test_igvm_agent_rpc_server used by VMM tests.
//! Intended for local runs where flowey is not starting the server globally.

#![cfg(windows)]
#![forbid(unsafe_code)]

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;

use anyhow::Context;

/// Name of the RPC server executable.
pub const RPC_SERVER_EXE: &str = "test_igvm_agent_rpc_server.exe";

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Checks if any process with the given executable name is running.
pub fn is_process_running(exe_name: &str) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {}", exe_name), "/NH"])
        .output();

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            !stdout.contains("INFO: No tasks")
        }
        Err(e) => {
            tracing::warn!("failed to run tasklist: {}", e);
            false
        }
    }
}

/// Ensures the RPC server process is running.
/// Returns Ok(()) if running, or an error if not.
pub fn ensure_rpc_server_running() -> anyhow::Result<()> {
    if is_process_running(RPC_SERVER_EXE) {
        tracing::info!(exe = RPC_SERVER_EXE, "RPC server is running");
        Ok(())
    } else {
        anyhow::bail!(
            "RPC server ({}) is not running. Start it before running tests or allow the test to start it.",
            RPC_SERVER_EXE
        )
    }
}

/// Guard that terminates the RPC server child process on drop.
pub struct RpcServerGuard(Child);

impl Drop for RpcServerGuard {
    fn drop(&mut self) {
        // If the process is still running, try to terminate it.
        match self.0.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(e) = self.0.kill() {
                    tracing::debug!("failed to kill RPC server child: {}", e);
                }
            }
            Err(e) => tracing::debug!("failed to query RPC server child: {}", e),
        }
    }
}

/// Starts the RPC server and returns a guard that will kill it when dropped.
/// Uses stdout EOF as a readiness signal (the server closes stdout once ready).
/// If the server exits immediately (e.g., endpoint already in use), this returns Ok(())
/// with a guard that will observe the exited process.
pub fn start_rpc_server(rpc_server_path: &Path) -> anyhow::Result<RpcServerGuard> {
    let mut child = Command::new(rpc_server_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .with_context(|| format!("failed to spawn {}", rpc_server_path.display()))?;

    // Wait for stdout to close as readiness signal
    let mut stdout = child
        .stdout
        .take()
        .context("failed to take stdout from RPC server")?;
    let mut byte = [0u8];
    let n = stdout
        .read(&mut byte)
        .context("failed to read from RPC server stdout")?;
    if n != 0 {
        anyhow::bail!(
            "expected RPC server stdout to close (EOF), but read {} bytes",
            n
        );
    }
    drop(stdout);

    // Give it a moment in case it needs to fail fast (e.g., endpoint already in use)
    thread::sleep(std::time::Duration::from_millis(50));

    match child.try_wait()? {
        Some(status) => {
            tracing::info!(
                exit_code = ?status.code(),
                "RPC server exited immediately (likely already running elsewhere)"
            );
        }
        None => {
            tracing::info!(pid = child.id(), "RPC server started and running");
        }
    }

    Ok(RpcServerGuard(child))
}
