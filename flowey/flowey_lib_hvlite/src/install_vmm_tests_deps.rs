// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V test pre-reqs

use flowey::node::prelude::*;
use std::collections::BTreeSet;

const HYPERV_TESTS_REQUIRED_FEATURES: [&str; 3] = [
    "Microsoft-Hyper-V",
    "Microsoft-Hyper-V-Management-PowerShell",
    "Microsoft-Hyper-V-Management-Clients",
];

const WHP_TESTS_REQUIRED_FEATURES: [&str; 1] = ["HypervisorPlatform"];

const VIRT_REG_PATH: &str = r#"HKLM\Software\Microsoft\Windows NT\CurrentVersion\Virtualization"#;

#[derive(Serialize, Deserialize, PartialEq)]
pub enum VmmTestsDepSelections {
    Windows {
        hyperv: bool,
        whp: bool,
        hardware_isolation: bool,
    },
    Linux,
}

flowey_request! {
    pub enum Request {
        /// Specify the necessary dependencies
        Select(VmmTestsDepSelections),
        /// Automatically install dependencies (requires admin privileges).
        ///
        /// When false, assume all dependencies are already present and skip
        /// checks that require admin privileges (e.g., DISM.exe).
        AutoInstall(bool),
        /// Install the dependencies
        Install(WriteVar<SideEffect>),
        /// Generate a list of commands that would install the dependencies
        GetCommands(WriteVar<Vec<String>>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut selections = None;
        let mut auto_install = None;
        let mut installed = Vec::new();
        let mut write_commands = Vec::new();
        for req in requests {
            match req {
                Request::Select(v) => same_across_all_reqs("Select", &mut selections, v)?,
                Request::AutoInstall(v) => {
                    same_across_all_reqs("AutoInstall", &mut auto_install, v)?
                }
                Request::Install(v) => installed.push(v),
                Request::GetCommands(v) => write_commands.push(v),
            }
        }
        let auto_install = auto_install;
        let installed = installed;
        let write_commands = write_commands;
        // Early return if no install or command requests - Select is not required in this case
        if installed.is_empty() && write_commands.is_empty() {
            return Ok(());
        }
        let selections = selections.ok_or(anyhow::anyhow!("Missing essential request: Select"))?;
        let installing = !installed.is_empty();

        match selections {
            VmmTestsDepSelections::Windows {
                hyperv,
                whp,
                hardware_isolation,
            } => {
                ctx.emit_rust_step("install vmm tests deps (windows)", move |ctx| {
                    installed.claim(ctx);
                    let write_commands = write_commands.claim(ctx);

                    move |rt| {
                        let mut commands = Vec::new();

                        if !matches!(rt.platform(), FlowPlatform::Windows)
                            && !flowey_lib_common::_util::running_in_wsl(rt)
                        {
                            anyhow::bail!("Must be on Windows or WSL2 to install Windows deps.")
                        }

                        // Resolve auto_install for local backend
                        let auto_install = match rt.backend() {
                            FlowBackend::Local => auto_install.ok_or_else(|| {
                                anyhow::anyhow!("Missing essential request: AutoInstall")
                            })?,
                            // CI backends always auto-install
                            FlowBackend::Ado | FlowBackend::Github => true,
                        };

                        // TODO: add these features and reg keys to the initial CI image

                        // Select required features
                        let mut features_to_enable = BTreeSet::new();
                        if hyperv {
                            features_to_enable.append(&mut HYPERV_TESTS_REQUIRED_FEATURES.into());
                        }
                        if whp {
                            features_to_enable.append(&mut WHP_TESTS_REQUIRED_FEATURES.into());
                        }

                        // Check if features are already enabled (requires admin, so skip if not auto_install)
                        if installing && auto_install && !features_to_enable.is_empty() {
                            let features = flowey::shell_cmd!(rt, "DISM.exe /Online /Get-Features").output()?;
                            assert!(features.status.success());
                            let features = String::from_utf8_lossy(&features.stdout).to_string();
                            let mut feature = None;
                            for line in features.lines() {
                                if let Some((k, v)) = line.split_once(":") {
                                    if let Some(f) = feature {
                                        assert_eq!(k.trim(), "State");
                                        match v.trim() {
                                            "Enabled" => {
                                                assert!(features_to_enable.remove(f));
                                            }
                                            "Disabled" => {}
                                            _ => anyhow::bail!("Unknown feature enablement state"),
                                        }
                                        feature = None;
                                    } else if k.trim() == "Feature Name" {
                                        let new_feature = v.trim();
                                        feature = features_to_enable.contains(new_feature).then_some(new_feature);
                                    }
                                }
                            }
                        } else if installing && !auto_install && !features_to_enable.is_empty() {
                            // Not auto-installing, assume features are already present
                            log::info!("Skipping Windows feature check (requires admin). Assuming features are already enabled.");
                            features_to_enable.clear();
                        }

                        // Prompt before enabling when running locally
                        if installing && auto_install && !features_to_enable.is_empty() && matches!(rt.backend(), FlowBackend::Local) {
                            let mut features_to_install_string = String::new();
                            for feature in features_to_enable.iter() {
                                features_to_install_string.push_str(feature);
                                features_to_install_string.push('\n');
                            }

                            log::warn!(
                                r#"
================================================================================
To run the VMM tests, the following features need to be enabled:
{features_to_install_string}

You may need to restart your system for the changes to take effect.

If you're OK with installing these features, please press <enter>.
Otherwise, press `ctrl-c` to cancel the run.
================================================================================
"#
                            );
                            let _ = std::io::stdin().read_line(&mut String::new());
                        }

                        // Install the features
                        for feature in features_to_enable {
                            if installing && auto_install {
                                flowey::shell_cmd!(rt, "DISM.exe /Online /NoRestart /Enable-Feature /All /FeatureName:{feature}").run()?;
                            }
                            commands.push(format!("DISM.exe /Online /NoRestart /Enable-Feature /All /FeatureName:{feature}"));
                        }

                        // Select required reg keys
                        let mut reg_keys_to_set = BTreeSet::new();
                        if hyperv {
                            // Allow loading IGVM from file (to run custom OpenHCL firmware)
                            reg_keys_to_set.insert("AllowFirmwareLoadFromFile");
                            // Enable COM3 and COM4 for Hyper-V VMs so we can get the OpenHCL KMSG logs over serial
                            reg_keys_to_set.insert("EnableAdditionalComPorts");

                            if hardware_isolation {
                                reg_keys_to_set.insert("EnableHardwareIsolation");
                            }
                        }

                        // Check if reg keys are set (skip if not auto_install, assume already set)
                        if installing && auto_install && !reg_keys_to_set.is_empty() {
                            let output = flowey::shell_cmd!(rt, "reg.exe query {VIRT_REG_PATH}").output()?;
                            if output.status.success() {
                                let output = String::from_utf8_lossy(&output.stdout).to_string();
                                for line in output.lines() {
                                    let components = line.split_whitespace().collect::<Vec<_>>();
                                    if components.len() == 3
                                        && reg_keys_to_set.contains(components[0])
                                        && components[1] == "REG_DWORD"
                                        && components[2] == "0x1"
                                    {
                                        assert!(reg_keys_to_set.remove(components[0]));
                                    }
                                }
                            }
                        } else if installing && !auto_install && !reg_keys_to_set.is_empty() {
                            // Not auto-installing, assume reg keys are already set
                            log::info!("Skipping registry key check. Assuming keys are already set.");
                            reg_keys_to_set.clear();
                        }

                        // Prompt before changing registry when running locally
                        if installing && auto_install && !reg_keys_to_set.is_empty() && matches!(rt.backend(), FlowBackend::Local) {
                            let mut reg_keys_to_set_string = String::new();
                            for feature in reg_keys_to_set.iter() {
                                reg_keys_to_set_string.push_str(feature);
                                reg_keys_to_set_string.push('\n');
                            }

                            log::warn!(
                                r#"
================================================================================
To run the VMM tests, the following registry keys need to be set to 1:
{reg_keys_to_set_string}

If you're OK with changing the registry, please press <enter>.
Otherwise, press `ctrl-c` to cancel the run.
================================================================================
"#
                            );
                            let _ = std::io::stdin().read_line(&mut String::new());
                        }

                        // Modify the registry
                        for v in reg_keys_to_set {
                            // TODO: figure out why reg.exe is not found if I
                            // render the command as a string first and share
                            if installing && auto_install {
                                flowey::shell_cmd!(rt, "reg.exe add {VIRT_REG_PATH} /v {v} /t REG_DWORD /d 1 /f").run()?;
                            }
                            commands.push(format!("reg.exe add \"{VIRT_REG_PATH}\" /v {v} /t REG_DWORD /d 1 /f"));
                        }

                        for write_cmds in write_commands {
                            rt.write(write_cmds, &commands);
                        }

                        Ok(())
                    }
                });
            }
            VmmTestsDepSelections::Linux => {
                ctx.emit_rust_step("install vmm tests deps (linux)", |ctx| {
                    installed.claim(ctx);
                    let write_commands = write_commands.claim(ctx);

                    |rt| {
                        for write_cmds in write_commands {
                            rt.write(write_cmds, &Vec::new());
                        }

                        Ok(())
                    }
                });
            }
        }

        Ok(())
    }
}
