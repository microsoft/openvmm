// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for interacting with Hyper-V VMs.

use anyhow::Context;
use anyhow::Ok;
use guid::Guid;
use pal_async::timer::PolledTimer;
use pal_async::DefaultDriver;
use std::ffi::OsStr;
use std::process::Stdio;
use std::time::Duration;

pub fn hvc_start(vmid: &Guid) -> anyhow::Result<()> {
    hvc_output(|cmd| cmd.arg("start").arg(vmid.to_string()))
        .map(|_| ())
        .context("hvc_start")
}

pub fn hvc_kill(vmid: &Guid) -> anyhow::Result<()> {
    hvc_output(|cmd| cmd.arg("kill").arg(vmid.to_string()))
        .map(|_| ())
        .context("hvc_kill")
}

/// HyperV VM state as reported by hvc
pub enum VmState {
    /// The VM is powered off.
    Off,
    /// The VM is powered on.
    On,
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
    /// Error getting the VM state.
    Unknown,
}

pub fn hvc_state(vmid: &Guid) -> anyhow::Result<VmState> {
    hvc_output(|cmd| cmd.arg("state").arg(vmid.to_string()))
        .map(|s| match s.trim_end() {
            "off" => VmState::Off,
            "on" => VmState::On,
            "starting" => VmState::Starting,
            "stopping" => VmState::Stopping,
            "saved" => VmState::Saved,
            "paused" => VmState::Paused,
            "resetting" => VmState::Resetting,
            "saving" => VmState::Saving,
            "pausing" => VmState::Pausing,
            "resuming" => VmState::Resuming,
            _ => VmState::Unknown,
        })
        .context("hvc_state")
}

pub async fn hvc_wait_for_power_off(driver: &DefaultDriver, vmid: &Guid) -> anyhow::Result<()> {
    const SHUTDOWN_TIMEOUT: usize = 20;
    let mut attempts = 0;
    while !matches!(hvc_state(vmid)?, VmState::Off) {
        if attempts >= SHUTDOWN_TIMEOUT {
            anyhow::bail!("VM shutdown timed out")
        }
        attempts += 1;
        PolledTimer::new(driver).sleep(Duration::from_secs(1)).await;
    }

    Ok(())
}

pub fn hvc_ensure_off(vmid: &Guid) -> anyhow::Result<()> {
    if !matches!(hvc_state(vmid)?, VmState::Off) {
        hvc_kill(vmid)?;
    }

    Ok(())
}

/// Runs hvc with the given arguments and returns the output.
fn hvc_output(
    f: impl FnOnce(&mut std::process::Command) -> &mut std::process::Command,
) -> anyhow::Result<String> {
    let mut cmd = std::process::Command::new("hvc.exe");
    cmd.stderr(Stdio::piped()).stdin(Stdio::null());
    f(&mut cmd);

    let output = cmd.output().expect("failed to launch hvc");

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
        anyhow::bail!("hvc failed with exit code: {}", output.status);
    }
    String::from_utf8(output.stdout).context("output is not utf-8")
}
