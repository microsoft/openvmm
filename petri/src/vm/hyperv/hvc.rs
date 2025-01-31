// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for interacting with Hyper-V VMs.

use anyhow::Context as _;
use anyhow::Ok;

pub fn hvc_start(vm: &str) -> anyhow::Result<()> {
    run_hvc(|cmd| cmd.arg("start").arg(vm))
}

pub fn hvc_kill(vm: &str) -> anyhow::Result<()> {
    run_hvc(|cmd| cmd.arg("kill").arg(vm))
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

pub fn hvc_state(vm: &str) -> VmState {
    hvc_output(|cmd| cmd.arg("state").arg(vm)).map_or_else(
        |_| VmState::Unknown,
        |s| match s.trim_end() {
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
        },
    )
}

pub fn hvc_list() -> anyhow::Result<Vec<String>> {
    let output = hvc_output(|cmd| cmd.arg("list").arg("-q"))?;
    Ok(output.lines().map(|l| l.to_owned()).collect())
}

pub fn hvc_wait_for_power_off(vm: &str) -> anyhow::Result<()> {
    while !matches!(hvc_state(vm), VmState::Off) {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    Ok(())
}

pub fn hvc_ensure_off(vm: &str) -> anyhow::Result<()> {
    if !matches!(hvc_state(vm), VmState::Off) {
        hvc_kill(vm)?;
    }

    Ok(())
}

/// Runs hvc with the given arguments.
fn run_hvc(
    f: impl FnOnce(&mut std::process::Command) -> &mut std::process::Command,
) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new("hvc.exe");
    f(&mut cmd);
    let status = cmd.status().context("failed to launch hvc")?;
    if !status.success() {
        anyhow::bail!("hvc failed with exit code: {}", status);
    }
    Ok(())
}

/// Runs hvc with the given arguments and returns the output.
fn hvc_output(
    f: impl FnOnce(&mut std::process::Command) -> &mut std::process::Command,
) -> anyhow::Result<String> {
    let mut cmd = std::process::Command::new("hvc.exe");
    f(&mut cmd);
    let output = cmd.output().expect("failed to launch hvc");
    if !output.status.success() {
        anyhow::bail!("hvc failed with exit code: {}", output.status);
    }
    String::from_utf8(output.stdout).context("output is not utf-8")
}
