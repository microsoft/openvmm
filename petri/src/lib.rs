// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A Rust-based testing framework for VMMs.
//!
//! At this time - `petri` supports testing OpenVMM, OpenHCL,
//! and Hyper-V based VMs.

// TODO: Remove this dependency by adding a frontend worker that handles
// hypervisor auto-detection in the spawned openvmm process instead of
// requiring probes to be registered in the petri process.
extern crate openvmm_hypervisors as _;

pub mod disk_image;
mod linux_direct_serial_agent;
// TODO: Add docs and maybe a trait interface for this, or maybe this can
// remain crate-local somehow without violating interface privacy.
#[expect(missing_docs)]
pub mod openhcl_diag;
pub mod requirements;
mod test;
mod tracing;
mod vm;
mod worker;

pub use petri_artifacts_core::ArtifactHandle;
pub use petri_artifacts_core::ArtifactResolver;
pub use petri_artifacts_core::ArtifactSource;
pub use petri_artifacts_core::AsArtifactHandle;
pub use petri_artifacts_core::ErasedArtifactHandle;
pub use petri_artifacts_core::RemoteAccess;
pub use petri_artifacts_core::ResolveTestArtifact;
pub use petri_artifacts_core::ResolvedArtifact;
pub use petri_artifacts_core::ResolvedArtifactSource;
pub use petri_artifacts_core::ResolvedOptionalArtifact;
pub use petri_artifacts_core::TestArtifactRequirements;
pub use petri_artifacts_core::TestArtifacts;
pub use pipette_client as pipette;
pub use test::PetriTestParams;
pub use test::RunTest;
pub use test::SimpleTest;
pub use test::TestCase;
pub use test::test_macro_support;
pub use test::test_main;
pub use tracing::*;
pub use vm::*;

use jiff::Timestamp;
use pal_async::process::PolledChild;
use std::ops::Deref;
use std::ops::DerefMut;
use std::process::Command;
use std::process::Stdio;
use thiserror::Error;

/// 1 kibibyte's worth of bytes.
pub const SIZE_1_KB: u64 = 1024;
/// 1 mebibyte's worth of bytes.
pub const SIZE_1_MB: u64 = 1024 * SIZE_1_KB;
/// 1 gibibyte's worth of bytes.
pub const SIZE_1_GB: u64 = 1024 * SIZE_1_MB;

/// The kind of shutdown to perform.
#[expect(missing_docs)] // Self-describing names.
pub enum ShutdownKind {
    Shutdown,
    Reboot,
    Hibernate,
}

/// Error running command
#[derive(Error, Debug)]
pub enum CommandError {
    /// failed to launch command
    #[error("failed to launch command")]
    Launch(#[from] std::io::Error),
    /// command exited with non-zero status
    #[error("command exited with non-zero status ({0}): {1}")]
    Command(std::process::ExitStatus, String),
}

/// Run a command on the host and return the output
pub async fn run_host_cmd(mut cmd: Command) -> Result<String, CommandError> {
    cmd.stderr(Stdio::piped()).stdin(Stdio::null());

    let cmd_debug = format!("{cmd:?}");
    ::tracing::debug!(cmd = cmd_debug, "executing command");

    let start = Timestamp::now();
    let output = blocking::unblock(move || cmd.output()).await?;
    let time_elapsed = Timestamp::now() - start;

    let stdout_str = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_str = String::from_utf8_lossy(&output.stderr).to_string();
    ::tracing::debug!(
        cmd = cmd_debug,
        stdout_str,
        stderr_str,
        "command exited in {:.3}s with status {}",
        time_elapsed.total(jiff::Unit::Second).unwrap(),
        output.status
    );

    if !output.status.success() {
        return Err(CommandError::Command(output.status, stderr_str));
    }

    Ok(stdout_str.trim().to_owned())
}

/// Owns a process launched by a test, killing it on drop.
///
/// [`std::process::Child`] deliberately does *not* kill the process when it is
/// dropped. Without this guard, any test that fails or panics before reaching
/// its `TeardownVM`/`Quit` calls leaves an orphaned process behind.
pub struct TestChild(PolledChild<std::process::Child>);

impl TestChild {
    /// Create a new test child that will be killed on drop
    pub fn new(child: PolledChild<std::process::Child>) -> Self {
        Self(child)
    }
}

impl Deref for TestChild {
    type Target = PolledChild<std::process::Child>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TestChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for TestChild {
    fn drop(&mut self) {
        let child = self.0.get_mut();
        // `kill` reports success for an already-reaped child, so ask `try_wait`
        // whether the process is actually gone rather than relying on that.
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        ::tracing::warn!(
            "killing test child process which was still running at the end of the test"
        );
        if let Err(err) = child.kill() {
            ::tracing::warn!(
                error = &err as &dyn std::error::Error,
                "failed to kill test child process"
            );
            return;
        }
        // Reap the process so it doesn't linger as a zombie. It was just
        // killed, so this returns promptly.
        let _ = child.wait();
    }
}
