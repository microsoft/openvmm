// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for managing the test_igvm_agent_rpc_server used by VMM tests.
//! Intended for local runs where flowey is not starting the server globally.

#![cfg(windows)]
#![forbid(unsafe_code)]

use std::path::Path;

use anyhow::Context;
use std::fs::File;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::process::Stdio;
use std::thread;

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

/// Starts the RPC server with stderr redirected to `log_file_path`.
/// Uses stdout EOF as a readiness signal (the server closes stdout once ready).
/// If the server exits immediately (e.g., endpoint already in use), this returns Ok(()).
pub fn start_rpc_server_with_logs(
    rpc_server_path: &Path,
    log_file_path: &Path,
) -> anyhow::Result<()> {
    let stderr_file = File::create(log_file_path)?;

    let mut child = Command::new(rpc_server_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr_file)
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
            Ok(())
        }
        None => {
            tracing::info!(
                pid = child.id(),
                log = %log_file_path.display(),
                "RPC server started and running"
            );
            // Drop the handle so the server keeps running even if caller exits.
            drop(child);
            Ok(())
        }
    }
}
