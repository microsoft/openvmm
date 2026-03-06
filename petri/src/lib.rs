// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A Rust-based testing framework for VMMs.
//!
//! At this time - `petri` supports testing OpenVMM, OpenHCL,
//! and Hyper-V based VMs.

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
pub use petri_artifacts_core::AsArtifactHandle;
pub use petri_artifacts_core::ErasedArtifactHandle;
pub use petri_artifacts_core::ResolveTestArtifact;
pub use petri_artifacts_core::ResolvedArtifact;
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
    // TODO: Add hibernate?
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
