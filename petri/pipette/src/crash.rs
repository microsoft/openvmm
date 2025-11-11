// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handler for kernel crash requests.

#[cfg(target_os = "linux")]
pub fn trigger_kernel_crash() -> anyhow::Result<()> {
    use anyhow::Context;

    std::fs::write("/proc/sysrq-trigger", "c").context("failed to write to /proc/sysrq-trigger")?;
    Ok(())
}

#[cfg(windows)]
pub fn trigger_kernel_crash() -> anyhow::Result<()> {
    use anyhow::Context;

    let output = std::process::Command::new("taskkill")
        .args(["/IM", "wininit.exe", "/F", "/T"])
        .output()
        .context("failed to execute taskkill")?;

    if !output.status.success() {
        anyhow::bail!(
            "taskkill exited with status {}:\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}
