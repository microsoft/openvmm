// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V test pre-reqs

use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

const HYPERV_TESTS_REQUIRED_FEATURES: [&str; 3] = [
    "Microsoft-Hyper-V",
    "Microsoft-Hyper-V-Management-PowerShell",
    "Microsoft-Hyper-V-Management-Clients",
];

const WHP_TESTS_REQUIRED_FEATURES: [&str; 1] = ["HypervisorPlatform"];

const VIRT_REG_PATH: &str = r#"HKLM\Software\Microsoft\Windows NT\CurrentVersion\Virtualization"#;
const HYPERVISOR_REG_PATH: &str = r#"HKLM\System\CurrentControlSet\Control\Hypervisor"#;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum VmmTestsDepSelections {
    Windows {
        hyperv: bool,
        whp: bool,
        hardware_isolation: bool,
    },
    Linux,
}

flowey_config! {
    /// Config for the install_vmm_tests_deps node.
    pub struct Config {
        /// Specify the necessary dependencies
        pub selections: Option<VmmTestsDepSelections>,
        /// Automatically install dependencies (requires admin privileges).
        ///
        /// When false, assume all dependencies are already present and skip
        /// checks that require admin privileges (e.g., DISM.exe).
        ///
        /// Must be set to true/false when running locally.
        pub auto_install: Option<bool>,
    }
}

flowey_request! {
    pub enum Request {
        /// Install the dependencies
        Install(WriteVar<SideEffect>),
        /// Generate a list of commands that would install the dependencies
        GetCommands(WriteVar<Vec<String>>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let mut installed = Vec::new();
        let mut write_commands = Vec::new();
        for req in requests {
            match req {
                Request::Install(v) => installed.push(v),
                Request::GetCommands(v) => write_commands.push(v),
            }
        }

        let installed = installed;
        let write_commands = write_commands;

        // Return if no requests specified
        if installed.is_empty() && write_commands.is_empty() {
            return Ok(());
        }

        let selections = config
            .selections
            .ok_or(anyhow::anyhow!("missing config: selections"))?;
        let auto_install = config.auto_install;
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
                        let mut reg_keys_to_set = BTreeMap::new();
                        if hyperv {
                            reg_keys_to_set.insert(VIRT_REG_PATH, BTreeMap::new());

                            // Allow loading IGVM from file (to run custom OpenHCL firmware)
                            reg_keys_to_set.get_mut(VIRT_REG_PATH).unwrap().insert("AllowFirmwareLoadFromFile", ("REG_DWORD", "0x1"));

                            // Enable COM3 and COM4 for Hyper-V VMs so we can get the OpenHCL KMSG logs over serial
                            reg_keys_to_set.get_mut(VIRT_REG_PATH).unwrap().insert("EnableAdditionalComPorts", ("REG_DWORD", "0x1"));

                            if hardware_isolation {
                                reg_keys_to_set.insert(HYPERVISOR_REG_PATH, BTreeMap::new());

                                reg_keys_to_set.get_mut(VIRT_REG_PATH).unwrap().insert("EnableHardwareIsolation", ("REG_DWORD", "0x1"));
                            }
                        }

                        // Check if reg keys are set (skip if not auto_install, assume already set)
                        if installing && auto_install && !reg_keys_to_set.is_empty() {
                            for (path, keys) in reg_keys_to_set.iter_mut() {
                                let output = flowey::shell_cmd!(rt, "reg.exe query {path}").output()?;
                                if output.status.success() {
                                    let output = String::from_utf8_lossy(&output.stdout).to_string();
                                    let mut existing_keys = BTreeMap::new();
                                    for line in output.lines() {
                                        let components = line.split_whitespace().collect::<Vec<_>>();
                                        if components.len() == 3 {
                                        existing_keys.insert(components[0], (components[1], components[2]));
                                        }
                                    }
                                    for (key, (existing_type, existing_value)) in existing_keys.iter() {
                                        if let Some((new_type, new_value)) = keys.get(key)
                                            
                                        {
                                            if *existing_type == *new_type
                                            && *existing_value == *new_value {
                                                assert!( keys.remove(key).is_some());
                                            }
                                        }
                                    }
                                }
                            }
                            
                        } else if installing && !auto_install && !reg_keys_to_set.is_empty() {
                            // Not auto-installing, assume reg keys are already set
                            log::info!("Skipping registry key check. Assuming keys are already set.");
                            reg_keys_to_set.clear();
                        }

                        // flatten the keys
                        let reg_keys_to_set = reg_keys_to_set.into_iter().flat_map(|(p, k)| k.into_iter().map(move |(v, (t, d))| (p, v, t, d))).collect::<Vec<_>>();
                        let mut reg_cmds = reg_keys_to_set.iter().map(|(p, v, t, d)| format!("reg.exe add \"{p}\" /v {v} /t {t} /d {d} /f")).collect::<Vec<_>>();

                        // Prompt before changing registry when running locally
                        if installing && auto_install && !reg_keys_to_set.is_empty() && matches!(rt.backend(), FlowBackend::Local) {
                            
                            let mut reg_keys_to_set_string = String::new();
                            for cmd in reg_cmds.iter() {
                                reg_keys_to_set_string.push_str(cmd);
                                reg_keys_to_set_string.push('\n');
                                
                            }

                            log::warn!(
                                r#"
================================================================================
To run the VMM tests, the following registry keys need to be set:
{reg_keys_to_set_string}

If you're OK with changing the registry, please press <enter>.
Otherwise, press `ctrl-c` to cancel the run.
================================================================================
"#
                            );
                            let _ = std::io::stdin().read_line(&mut String::new());
                        }

                        // Modify the registry
                        commands.append(&mut reg_cmds);
                        for (p, v, t, d) in reg_keys_to_set {
                            // TODO: figure out why reg.exe is not found if I
                            // render the command as a string first and share
                            if installing && auto_install {
                                flowey::shell_cmd!(rt, "reg.exe add \"{p}\" /v {v} /t {t} /d {d} /f").run()?;
                            }
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
