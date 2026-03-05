// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handler for the power off request.

// UNSAFETY: required for Windows shutdown API
#![cfg_attr(windows, expect(unsafe_code))]

#[cfg(target_os = "linux")]
pub fn handle_shutdown(request: pipette_protocol::ShutdownRequest) -> anyhow::Result<()> {
    use anyhow::Context;

    let program = match request.shutdown_type {
        pipette_protocol::ShutdownType::PowerOff => "poweroff",
        pipette_protocol::ShutdownType::Reboot => "reboot",
    };
    let mut command = std::process::Command::new(program);

    // Check if we should use the force flag. This is needed when:
    // 1. init is just a shell (linux-direct boot)
    // 2. We're not running under systemd (e.g., OpenRC on Alpine)
    let init_cmdline = std::fs::read("/proc/1/cmdline").context("failed to read cmdline")?;
    let is_shell_init = init_cmdline.starts_with(b"/bin/sh");
    let is_systemd = std::path::Path::new("/run/systemd/system").exists();

    if is_shell_init || !is_systemd {
        command.arg("-f");
    }

    let output = command
        .output()
        .with_context(|| format!("failed to launch {}", program))?;
    if output.status.success() {
        Ok(())
    } else {
        anyhow::bail!("failed to power off: {}", output.status);
    }
}

#[cfg(windows)]
pub fn handle_shutdown(request: pipette_protocol::ShutdownRequest) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use std::ptr::null_mut;
    use windows_sys::Wdk::System::SystemServices::SE_SHUTDOWN_PRIVILEGE;
    use windows_sys::Win32::Foundation::LUID;
    use windows_sys::Win32::Security::AdjustTokenPrivileges;
    use windows_sys::Win32::Security::LUID_AND_ATTRIBUTES;
    use windows_sys::Win32::Security::SE_PRIVILEGE_ENABLED;
    use windows_sys::Win32::Security::TOKEN_ADJUST_PRIVILEGES;
    use windows_sys::Win32::Security::TOKEN_PRIVILEGES;
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::System::Shutdown::InitiateShutdownW;
    use windows_sys::Win32::System::Shutdown::SHTDN_REASON_FLAG_PLANNED;
    use windows_sys::Win32::System::Shutdown::SHTDN_REASON_MAJOR_OTHER;
    use windows_sys::Win32::System::Shutdown::SHTDN_REASON_MINOR_OTHER;
    use windows_sys::Win32::System::Shutdown::SHUTDOWN_FORCE_OTHERS;
    use windows_sys::Win32::System::Shutdown::SHUTDOWN_FORCE_SELF;
    use windows_sys::Win32::System::Shutdown::SHUTDOWN_GRACE_OVERRIDE;
    use windows_sys::Win32::System::Shutdown::SHUTDOWN_POWEROFF;
    use windows_sys::Win32::System::Shutdown::SHUTDOWN_RESTART;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::OpenProcessToken;

    // Enable the shutdown privilege on the current process.

    // SAFETY: calling as documented
    let token = unsafe {
        let mut token = null_mut();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        );
        OwnedHandle::from_raw_handle(token)
    };

    let tkp = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: LUID {
                LowPart: SE_SHUTDOWN_PRIVILEGE as u32,
                HighPart: 0,
            },
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };

    // SAFETY: calling as documented with an appropriate initialized struct.
    let r =
        unsafe { AdjustTokenPrivileges(token.as_raw_handle(), 0, &tkp, 0, null_mut(), null_mut()) };
    if r == 0 {
        return Err(std::io::Error::last_os_error()).context("failed to adjust token privileges");
    }

    let flag = match request.shutdown_type {
        pipette_protocol::ShutdownType::PowerOff => SHUTDOWN_POWEROFF,
        pipette_protocol::ShutdownType::Reboot => SHUTDOWN_RESTART,
    };

    // SAFETY: calling as documented
    let win32_err = unsafe {
        InitiateShutdownW(
            null_mut(),
            null_mut(),
            0,
            SHUTDOWN_GRACE_OVERRIDE | SHUTDOWN_FORCE_SELF | SHUTDOWN_FORCE_OTHERS | flag,
            SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_MINOR_OTHER | SHTDN_REASON_FLAG_PLANNED,
        )
    };
    if win32_err != 0 {
        return Err(std::io::Error::from_raw_os_error(win32_err as i32))
            .context("failed to initiate shutdown");
    }
    Ok(())
}

#[cfg(windows)]
#[expect(dead_code)] // Currently unused, but left as an example
pub fn start_shutdown_trace() -> anyhow::Result<()> {
    use anyhow::Context;

    std::fs::write("shutdown.wprp", include_bytes!("../shutdown.wprp")).context("writing wprp")?;

    let trace_start_res = std::process::Command::new("wpr")
        .args(["-start", "shutdown.wprp", "-filemode"])
        .output()
        .context("calling wpr")?;

    if !trace_start_res.status.success() {
        tracing::error!(
            stdout = String::from_utf8_lossy(&trace_start_res.stdout).to_string(),
            stderr = String::from_utf8_lossy(&trace_start_res.stderr).to_string(),
            status = ?trace_start_res.status,
            "failed to start shutdown trace"
        );
        anyhow::bail!("failed to start shutdown trace");
    } else {
        tracing::info!("started shutdown trace");
    }

    Ok(())
}

#[cfg(windows)]
#[expect(dead_code)] // Currently unused, but left as an example
pub async fn send_shutdown_trace(
    diag_file_send: crate::agent::DiagnosticSender,
) -> anyhow::Result<()> {
    use anyhow::Context;

    let trace_stop_res = std::process::Command::new("wpr")
        .args(["-stop", "shutdown_trace.etl"])
        .output()?;
    tracing::info!(
        stdout = String::from_utf8_lossy(&trace_stop_res.stdout).to_string(),
        stderr = String::from_utf8_lossy(&trace_stop_res.stderr).to_string(),
        status = ?trace_stop_res.status,
        "stopped shutdown trace"
    );

    diag_file_send
        .send("shutdown_trace.etl")
        .await
        .context("failed to send shutdown trace file")?;

    Ok(())
}
