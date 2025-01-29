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

                    // TODO: add this to the initial CI image (and maybe the reg keys too)
                    xshell::cmd!(sh, "DISM /Online /Norestart /Enable-Feature /All /FeatureName:Microsoft-Hyper-V-Management-PowerShell").run()?;

                    let firmware_load_path = r#"HKLM\Software\Microsoft\Windows NT\CurrentVersion\Virtualization"#;
                    xshell::cmd!(sh, "reg add {firmware_load_path} /v AllowFirmwareLoadFromFile /t REG_DWORD /d 1 /f").run()?;

                    Ok(())
                }
            });
        } else {
            ctx.emit_side_effect_step([], requests.into_iter().map(|x| x.0));
        }

        Ok(())
    }
}
