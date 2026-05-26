// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V test pre-reqs

use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::process::Stdio;

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
    Windows(VmmTestsDepSelectionsWindows),
    Linux,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct VmmTestsDepSelectionsWindows {
    pub hyperv: bool,
    pub whp: bool,
    pub hardware_isolation: bool,
}

flowey_config! {
    /// Config for the install_vmm_tests_deps node.
    pub struct Config {
        /// Specify the necessary dependencies
        pub selections: Option<VmmTestsDepSelections>,
        /// Automatically install dependencies (requires admin privileges).
        ///
        /// When false, skip checks that require admin privileges.
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
            VmmTestsDepSelections::Windows(selections) => {
                ctx.emit_rust_step("install vmm tests deps (windows)", move |ctx| {
                    installed.claim(ctx);
                    let write_commands = write_commands.claim(ctx);

                    move |rt| {
                        install_windows_deps(
                            rt,
                            installing,
                            auto_install,
                            selections,
                            write_commands,
                        )
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

fn install_windows_deps(
    rt: &mut RustRuntimeServices<'_>,
    installing: bool,
    auto_install: Option<bool>,
    selections: VmmTestsDepSelectionsWindows,
    write_commands: Vec<WriteVar<Vec<String>, VarClaimed>>,
) -> anyhow::Result<()> {
    let VmmTestsDepSelectionsWindows {
        hyperv,
        whp,
        hardware_isolation,
    } = selections;
    let mut commands = Vec::new();
    let mut needs_restart = false;

    if !matches!(rt.platform(), FlowPlatform::Windows)
        && !flowey_lib_common::_util::running_in_wsl(rt)
    {
        anyhow::bail!("Must be on Windows or WSL2 to install Windows deps.")
    }

    // Resolve auto_install for local backend
    let auto_install = match rt.backend() {
        FlowBackend::Local => {
            auto_install.ok_or_else(|| anyhow::anyhow!("Missing essential request: AutoInstall"))?
        }
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

    // write commands for vmm_tests_run build only mode
    for feature in features_to_enable.iter() {
        commands.push(format!(
            "DISM.exe /Online /NoRestart /Enable-Feature /All /FeatureName:{feature}"
        ));
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
                    feature = features_to_enable
                        .contains(new_feature)
                        .then_some(new_feature);
                }
            }
        }
    } else if installing && !auto_install && !features_to_enable.is_empty() {
        if powershell_builder::PowerShellBuilder::new()
            .cmdlet("Get-VM")
            .finish()
            .build()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            log::info!(
                "Verified that Hyper-V is installed, assuming related features are enabled."
            );
            log::info!(
                "If you encounter issues, try re-running in an Administrator window with `--install-missing-deps`"
            );
        } else {
            anyhow::bail!(
                "Hyper-V is not installed or your user account is not in the \"Hyper-V Administrators\" group. Re-run in an Administrator window with `--install-missing-deps`"
            );
        }

        features_to_enable.clear();
    }

    // Prompt before enabling when running locally
    if installing
        && auto_install
        && !features_to_enable.is_empty()
        && matches!(rt.backend(), FlowBackend::Local)
    {
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

        needs_restart = true;
    }

    // Install the features
    for feature in features_to_enable {
        if installing && auto_install {
            flowey::shell_cmd!(
                rt,
                "DISM.exe /Online /NoRestart /Enable-Feature /All /FeatureName:{feature}"
            )
            .run()?;
        }
    }

    // Select required reg keys
    let mut reg_keys_to_set = BTreeMap::new();
    if hyperv {
        // Allow loading IGVM from file (to run custom OpenHCL firmware)
        reg_keys_to_set
            .entry(VIRT_REG_PATH)
            .or_insert(BTreeMap::new())
            .insert("AllowFirmwareLoadFromFile", ("REG_DWORD", "0x1", false));

        // Enable COM3 and COM4 for Hyper-V VMs so we can get the OpenHCL KMSG logs over serial
        reg_keys_to_set
            .entry(VIRT_REG_PATH)
            .or_insert(BTreeMap::new())
            .insert("EnableAdditionalComPorts", ("REG_DWORD", "0x1", false));

        if hardware_isolation {
            reg_keys_to_set
                .entry(HYPERVISOR_REG_PATH)
                .or_insert(BTreeMap::new())
                .insert("EnableHardwareIsolation", ("REG_DWORD", "0x1", true));
        }
    }

    // write commands for vmm_tests_run build only mode
    for (p, k) in reg_keys_to_set.iter() {
        for (v, (t, d, _)) in k {
            commands.push(format!("reg.exe add \"{p}\" /v {v} /t {t} /d {d} /f"));
        }
    }

    // Check if reg keys are set
    if installing && !reg_keys_to_set.is_empty() {
        for (path, keys) in reg_keys_to_set.iter_mut() {
            let output = flowey::shell_cmd!(rt, "reg.exe query {path}").output()?;
            if output.status.success() {
                let output = String::from_utf8_lossy(&output.stdout).to_string();
                for line in output.lines() {
                    let components = line.split_whitespace().collect::<Vec<_>>();
                    if components.len() == 3
                        && keys
                            .get(components[0])
                            .is_some_and(|(t, d, _)| *t == components[1] && *d == components[2])
                    {
                        assert!(keys.remove(components[0]).is_some());
                    }
                }
            }
        }
    }

    // flatten the keys
    let reg_keys_would_require_restart = reg_keys_to_set
        .iter()
        .any(|(_, k)| k.iter().any(|(_, (_, _, needs_restart))| *needs_restart));
    let reg_keys_to_set = reg_keys_to_set
        .into_iter()
        .flat_map(|(p, k)| k.into_iter().map(move |(v, (t, d, _))| (p, v, t, d)))
        .collect::<Vec<_>>();

    // Prompt before changing registry when running locally
    if installing && !reg_keys_to_set.is_empty() && matches!(rt.backend(), FlowBackend::Local) {
        let mut reg_keys_to_set_string = String::new();
        for (p, v, _, _) in reg_keys_to_set.iter() {
            reg_keys_to_set_string.push_str(p);
            reg_keys_to_set_string.push(' ');
            reg_keys_to_set_string.push_str(v);
            reg_keys_to_set_string.push('\n');
        }

        if auto_install {
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

            needs_restart |= reg_keys_would_require_restart;
        } else {
            anyhow::bail!(
                r#"
================================================================================
To run the VMM tests, the following registry keys need to be set:
{reg_keys_to_set_string}

Please re-run in an Administrator window with `--install-missing-deps`.
================================================================================
"#
            );
        }
    }

    // Modify the registry
    for (p, v, t, d) in reg_keys_to_set {
        if installing && auto_install {
            flowey::shell_cmd!(rt, "reg.exe add {p} /v {v} /t {t} /d {d} /f").run()?;
        }
    }

    if needs_restart {
        anyhow::bail!(
            "Installed dependencies require a restart. Please restart and re-run this command"
        );
    }

    for write_cmds in write_commands {
        rt.write(write_cmds, &commands);
    }

    Ok(())
}
