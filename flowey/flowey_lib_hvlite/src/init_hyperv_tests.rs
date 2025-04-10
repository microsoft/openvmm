// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V test pre-reqs

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request(pub WriteVar<SideEffect>);
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_openvmm_magicpath::Node>();
        ctx.import::<flowey_lib_common::download_protoc::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        if matches!(ctx.platform(), FlowPlatform::Windows) {
            ctx.emit_rust_step("init hyperv tests", move |ctx| {
                requests.into_iter().for_each(|x| {
                    x.0.claim(ctx);
                });
                |_| {
                    let sh = xshell::Shell::new()?;

                    // TODO: perform these operation on the reference CI image

                    // Install the Hyper-V Powershell commands
                    xshell::cmd!(sh, "DISM /Online /Norestart /Enable-Feature /All /FeatureName:Microsoft-Hyper-V-Management-PowerShell").run()?;

                    // Allow loading IGVM from file (to run custom OpenHCL firmware)
                    let firmware_load_path = r#"HKLM\Software\Microsoft\Windows NT\CurrentVersion\Virtualization"#;
                    xshell::cmd!(sh, "reg add {firmware_load_path} /v AllowFirmwareLoadFromFile /t REG_DWORD /d 1 /f").run()?;

                    // Enable COM3 and COM4 for Hyper-V VMs so we can get the OpenHCL KMSG logs over serial
                    xshell::cmd!(sh, "Enable-VelocityFeature -Feature 21938063").run()?;

                    Ok(())
                }
            });
        } else {
            ctx.emit_side_effect_step([], requests.into_iter().map(|x| x.0));
        }

        Ok(())
    }
}
