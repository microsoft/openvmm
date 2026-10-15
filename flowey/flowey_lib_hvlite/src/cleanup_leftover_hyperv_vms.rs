// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Remove Hyper-V VMs left behind by a previous test run.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// Completion indicator
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { done } = request;

        // Only meaningful on a dedicated CI machine: locally this would tear
        // down VMs the developer cares about.
        if matches!(ctx.backend(), FlowBackend::Local)
            || !matches!(ctx.platform(), FlowPlatform::Windows)
        {
            ctx.emit_side_effect_step([], [done]);
            return Ok(());
        }

        ctx.emit_rust_step("remove leftover Hyper-V VMs", |ctx| {
            done.claim(ctx);
            move |_rt| {
                let vms = powershell_builder::PowerShellBuilder::new()
                    .cmdlet("Get-VM")
                    .finish()
                    .build()
                    .output()?;
                log::info!(
                    "removing any existing VMs: {}",
                    String::from_utf8_lossy(&vms.stdout)
                );

                powershell_builder::PowerShellBuilder::new()
                    .cmdlet("Get-VM")
                    .pipeline()
                    .cmdlet("Stop-VM")
                    .flag("TurnOff")
                    .finish()
                    .build()
                    .output()?;

                powershell_builder::PowerShellBuilder::new()
                    .cmdlet("Get-VM")
                    .pipeline()
                    .cmdlet("Remove-VM")
                    .flag("Force")
                    .finish()
                    .build()
                    .output()?;

                // Remove-VM returns before the worker processes exit, and until
                // they do they still hold the VMs' disks open.
                powershell_builder::PowerShellBuilder::new()
                    .cmdlet("Wait-Process")
                    .arg("Name", "vmwp")
                    .arg("Timeout", "60")
                    .arg("ErrorAction", "SilentlyContinue")
                    .finish()
                    .build()
                    .output()?;

                Ok(())
            }
        });

        Ok(())
    }
}
