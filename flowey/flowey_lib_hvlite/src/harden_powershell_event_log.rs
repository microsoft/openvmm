// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Harden the Windows CI runner against transient PowerShell startup crashes.
//!
//! Windows PowerShell 5.1 (`powershell.exe`) initializes an
//! `EventLogLogProvider` during session startup that writes to the
//! `Windows PowerShell` Event Log source. On some CI runners the source is
//! either unregistered or has corrupted ACLs, which causes
//! `EventLog.SourceExists` / `EventLog.WriteEvent` to throw
//! `System.AccessViolationException` *before* any user command is dispatched.
//!
//! This node disables Windows PowerShell engine event logging via the
//! per-machine registry switch documented for the PowerShell engine. Once
//! disabled, `EventLogLogProvider.LogEvent` becomes a no-op and the AV no
//! longer occurs. The change is local to the runner and has no effect on
//! production hosts.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// Completion indicator.
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { done } = request;

        // Only meaningful on Windows CI runners.
        if !matches!(ctx.platform(), FlowPlatform::Windows) {
            ctx.emit_side_effect_step([], [done]);
            return Ok(());
        }

        ctx.emit_rust_step("harden Windows PowerShell event log", |ctx| {
            done.claim(ctx);
            move |rt| {
                if matches!(rt.backend(), FlowBackend::Local) {
                    // Don't tamper with a developer's local machine.
                    return Ok(());
                }

                // Disable Windows PowerShell engine event logging. The key may
                // not pre-exist on all runner images, so create it first.
                // Failures are logged but non-fatal: the worst case is we fall
                // back to the original (occasionally flaky) behavior.
                let script = r#"
                    $ErrorActionPreference = 'Continue'
                    $key = 'HKLM:\Software\Microsoft\PowerShell\1\PowerShellEngine'
                    if (-not (Test-Path $key)) {
                        New-Item -Path $key -Force | Out-Null
                    }
                    Set-ItemProperty -Path $key -Name 'EnableEventLogging' -Value 0 -Type DWord -Force
                    # Ensure the 'Windows PowerShell' Event Log source exists with
                    # the right registration so any path that still tries to use
                    # it doesn't fault.
                    try {
                        if (-not [System.Diagnostics.EventLog]::SourceExists('PowerShell')) {
                            [System.Diagnostics.EventLog]::CreateEventSource('PowerShell','Windows PowerShell')
                        }
                    } catch {
                        Write-Host "warn: could not verify PowerShell event source: $_"
                    }
                "#;

                let output = std::process::Command::new("powershell.exe")
                    .arg("-NoProfile")
                    .arg("-NonInteractive")
                    .arg("-ExecutionPolicy")
                    .arg("Bypass")
                    .arg("-Command")
                    .arg(script)
                    .output()?;

                if !output.status.success() {
                    log::warn!(
                        "failed to harden PowerShell event log (continuing): {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                } else {
                    log::info!("disabled Windows PowerShell engine event logging on this runner");
                }

                Ok(())
            }
        });

        Ok(())
    }
}
