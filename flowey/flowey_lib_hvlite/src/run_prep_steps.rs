// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Raw bindings to `prep_steps`, used to prepare test images before running tests.

use crate::build_prep_steps::PrepStepsOutput;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// Path to prep_steps bin to use
        pub prep_steps: ReadVar<PrepStepsOutput>,
        /// Environment variables to set when running prep_steps
        pub env: ReadVar<BTreeMap<String, String>>,
        /// Completion indicator
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            prep_steps,
            env,
            done,
        } = request;

        ctx.emit_rust_step("running vmm_test prep_steps", |ctx| {
            let prep_steps = prep_steps.claim(ctx);
            let env = env.claim(ctx);
            done.claim(ctx);
            move |rt| {
                let prep_steps = rt.read(prep_steps);
                let env = rt.read(env);

                #[cfg(windows)]
                if !matches!(rt.backend(), FlowBackend::Local) {
                    // Shutdown and remove any running VMs that might be using the disk
                    // generated during a previous test run. (CI only)
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
                }

                let sh = xshell::Shell::new()?;
                let binary_path = match prep_steps {
                    PrepStepsOutput::WindowsBin { exe, .. } => exe,
                    PrepStepsOutput::LinuxBin { bin, .. } => bin,
                };
                xshell::cmd!(sh, "{binary_path}").envs(env).run()?;

                Ok(())
            }
        });

        Ok(())
    }
}
