// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for interacting with Hyper-V VMs.

use anyhow::Context as _;
use powershell_builder::PowerShellBuilder;

/// Runs hcsdiag with the given arguments.
pub fn run_hcsdiag(
    f: impl FnOnce(&mut std::process::Command) -> &mut std::process::Command,
) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new("hcsdiag.exe");
    f(&mut cmd);
    let status = cmd.status().context("failed to launch hcsdiag")?;
    if !status.success() {
        anyhow::bail!("hcsdiag failed with exit code: {}", status);
    }
    Ok(())
}

/// Runs hvc with the given arguments.
pub fn run_hvc(
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
pub fn hvc_output(
    f: impl FnOnce(&mut std::process::Command) -> &mut std::process::Command,
) -> anyhow::Result<String> {
    let mut cmd = std::process::Command::new("hvc.exe");
    f(&mut cmd);
    let output = cmd.output().context("failed to launch hvc")?;
    if !output.status.success() {
        anyhow::bail!("hvc failed with exit code: {}", output.status);
    }
    String::from_utf8(output.stdout).context("output is not utf-8")
}

/// Runs a PowerShell script with the given arguments bound to its `param`
/// block.
///
/// `script` is emitted verbatim and is therefore trusted; `args` are quoted
/// and are not.
pub fn powershell_script(script: &str, args: &[&str]) -> anyhow::Result<String> {
    let mut cmdlet = PowerShellBuilder::new().cmdlet(format!("&{{{script}}}"));
    for arg in args {
        cmdlet = cmdlet.positional(arg);
    }
    let output = cmdlet
        .finish()
        .build()
        .output()
        .context("failed to launch powershell")?;
    if !output.status.success() {
        anyhow::bail!(
            "powershell failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).context("output is not utf-8")
}
