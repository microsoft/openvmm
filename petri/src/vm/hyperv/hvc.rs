// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for interacting with Hyper-V VMs.

use super::CommandError;
use anyhow::Context;
use guid::Guid;
use std::ffi::OsStr;
use std::process::Stdio;

pub fn hvc_start(vmid: &Guid) -> Result<(), CommandError> {
    hvc_output(|cmd| cmd.arg("start").arg(vmid.to_string())).map(|_| ())
}

pub fn hvc_stop(vmid: &Guid) -> Result<(), CommandError> {
    hvc_output(|cmd| cmd.arg("stop").arg(vmid.to_string())).map(|_| ())
}

pub fn hvc_kill(vmid: &Guid) -> Result<(), CommandError> {
    hvc_output(|cmd| cmd.arg("kill").arg(vmid.to_string())).map(|_| ())
}

pub fn hvc_restart(vmid: &Guid) -> Result<(), CommandError> {
    hvc_output(|cmd| cmd.arg("restart").arg(vmid.to_string())).map(|_| ())
}

pub fn hvc_reset(vmid: &Guid) -> Result<(), CommandError> {
    hvc_output(|cmd| cmd.arg("reset").arg(vmid.to_string())).map(|_| ())
}

/// HyperV VM state as reported by hvc
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum VmState {
    /// The VM is powered off.
    Off,
    /// The VM is powered on.
    Running,
    /// The VM is powering on.
    Starting,
    /// The VM is powering off.
    Stopping,
    /// The VM has been saved.
    Saved,
    /// The VM has been paused.
    Paused,
    /// The VM is being reset.
    Resetting,
    /// The VM is saving.
    Saving,
    /// The VM is pausing.
    Pausing,
    /// The VM is resuming.
    Resuming,
}

pub fn hvc_state(vmid: &Guid) -> anyhow::Result<VmState> {
    Ok(
        match hvc_output(|cmd| cmd.arg("state").arg(vmid.to_string()))
            .context("hvc_state")?
            .as_str()
        {
            "off" => VmState::Off,
            "running" => VmState::Running,
            "starting" => VmState::Starting,
            "stopping" => VmState::Stopping,
            "saved" => VmState::Saved,
            "paused" => VmState::Paused,
            "resetting" => VmState::Resetting,
            "saving" => VmState::Saving,
            "pausing" => VmState::Pausing,
            "resuming" => VmState::Resuming,
            s => anyhow::bail!("unknown vm state: {s}"),
        },
    )
}

pub fn hvc_ensure_off(vmid: &Guid) -> anyhow::Result<()> {
    for _ in 0..5 {
        if matches!(hvc_state(vmid)?, VmState::Off) {
            return Ok(());
        }
        if let Err(e) = hvc_kill(vmid) {
            tracing::warn!("hvc_kill attempt failed: {e}")
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    anyhow::bail!("Failed to stop VM")
}

/// Runs hvc with the given arguments and returns the output.
fn hvc_output(
    f: impl FnOnce(&mut std::process::Command) -> &mut std::process::Command,
) -> Result<String, CommandError> {
    let mut cmd = std::process::Command::new("hvc.exe");
    cmd.stderr(Stdio::piped()).stdin(Stdio::null());
    f(&mut cmd);

    let output = cmd.output()?;

    let hvc_cmd = format!(
        "{} {}",
        cmd.get_program().to_string_lossy(),
        cmd.get_args()
            .collect::<Vec<_>>()
            .join(OsStr::new(" "))
            .to_string_lossy()
    );
    let hvc_stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let hvc_stderr = String::from_utf8_lossy(&output.stderr).to_string();

    tracing::debug!(hvc_cmd, hvc_stdout, hvc_stderr);
    if !output.status.success() {
        return Err(CommandError::Command(output.status, hvc_stderr));
    }

    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
