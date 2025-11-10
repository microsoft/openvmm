// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(target_os = "linux")]
pub fn trigger_kernel_crash() -> anyhow::Result<()> {
    use anyhow::Context;

    std::fs::write("/proc/sysrq-trigger", "c").context("failed to write to /proc/sysrq-trigger")?;
    Ok(())
}

#[cfg(windows)]
pub fn trigger_kernel_crash() -> anyhow::Result<()> {
    use anyhow::Context;

    let status = std::process::Command::new("taskkill")
        .args(["/IM", "winlogon.exe", "/F"])
        .status()
        .context("failed to execute taskkill")?;

    if !status.success() {
        anyhow::bail!("taskkill exited with status {}", status);
    }

    Ok(())
}
