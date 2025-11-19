// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for interacting with Hyper-V VMs.

use crate::CommandError;
use anyhow::Context;
use guid::Guid;

pub async fn hvc_start(vmid: &Guid) -> Result<(), CommandError> {
    hvc_output(|cmd| cmd.arg("start").arg(vmid.to_string()))
        .await
        .map(|_| ())
}

pub async fn hvc_stop(vmid: &Guid) -> Result<(), CommandError> {
    hvc_output(|cmd| cmd.arg("stop").arg(vmid.to_string()))
        .await
        .map(|_| ())
}

pub async fn hvc_kill(vmid: &Guid) -> Result<(), CommandError> {
    hvc_output(|cmd| cmd.arg("kill").arg(vmid.to_string()))
        .await
        .map(|_| ())
}

pub async fn hvc_restart(vmid: &Guid) -> Result<(), CommandError> {
    hvc_output(|cmd| cmd.arg("restart").arg(vmid.to_string()))
        .await
        .map(|_| ())
}

pub async fn hvc_reset(vmid: &Guid) -> Result<(), CommandError> {
    hvc_output(|cmd| cmd.arg("reset").arg(vmid.to_string()))
        .await
        .map(|_| ())
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

pub async fn hvc_state(vmid: &Guid) -> anyhow::Result<VmState> {
    Ok(
        match hvc_output(|cmd| cmd.arg("state").arg(vmid.to_string()))
            .await
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

pub async fn hvc_ensure_off(vmid: &Guid) -> anyhow::Result<()> {
    for _ in 0..5 {
        if matches!(hvc_state(vmid).await?, VmState::Off) {
            return Ok(());
        }
        if let Err(e) = hvc_kill(vmid).await {
            tracing::warn!("hvc_kill attempt failed: {e}")
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    anyhow::bail!("Failed to stop VM")
}

/// Runs hvc with the given arguments and returns the output.
async fn hvc_output(
    f: impl FnOnce(&mut std::process::Command) -> &mut std::process::Command,
) -> Result<String, CommandError> {
    let mut cmd = std::process::Command::new("hvc.exe");
    f(&mut cmd);

    crate::run_host_cmd(cmd).await
}
