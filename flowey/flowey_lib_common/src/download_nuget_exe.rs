// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download `nuget.exe`

use flowey::node::prelude::*;
use std::fs;

#[derive(Serialize, Deserialize)]
pub enum NugetInstallPlatform {
    Windows,
    Linux,
    MacOs,
}

flowey_request! {
    pub enum Request {
        NugetBin(WriteVar<PathBuf>),
        NugetInstallPlatform(WriteVar<NugetInstallPlatform>),
        /// When running using WSL2, use `mono` to run the `nuget.exe` inside
        /// WSL2 directly, instead of running `nuget.exe` via WSL2 interop.
        ///
        /// This is sometimes required to work around windows defender bugs when
        /// restoring.
        LocalOnlyForceWsl2MonoNugetExe(bool),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::ado_task_nuget_tool_installer::Node>();
        ctx.import::<crate::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut broadcast_nuget_tool_kind = Vec::new();
        let mut broadcast_nuget_config_platform = Vec::new();
        let mut force_mono_nuget_exe_wsl2 = None;

        for req in requests {
            match req {
                Request::LocalOnlyForceWsl2MonoNugetExe(v) => same_across_all_reqs(
                    "LocalOnlyForceWsl2MonoNugetExe",
                    &mut force_mono_nuget_exe_wsl2,
                    v,
                )?,
                Request::NugetBin(outvar) => broadcast_nuget_tool_kind.push(outvar),
                Request::NugetInstallPlatform(outvar) => {
                    broadcast_nuget_config_platform.push(outvar)
                }
            };
        }

        let broadcast_nuget_tool_kind = broadcast_nuget_tool_kind;
        let broadcast_nuget_config_platform = broadcast_nuget_config_platform;

        let force_mono_nuget_exe_wsl2 = if matches!(ctx.backend(), FlowBackend::Local) {
            force_mono_nuget_exe_wsl2.ok_or(anyhow::anyhow!(
                "Missing essential request: LocalOnlyForceWsl2MonoNugetExe"
            ))?
        } else {
            if force_mono_nuget_exe_wsl2.is_some() {
                anyhow::bail!(
                    "can only use `LocalOnlyForceWsl2MonoNugetExe` when using the Local backend"
                );
            }
            false
        };

        // -- end of req processing -- //

        if !broadcast_nuget_config_platform.is_empty() {
            ctx.emit_rust_step("report nuget install platform", |ctx| {
                let broadcast_nuget_config_platform = broadcast_nuget_config_platform.claim(ctx);
                move |rt| {
                    let nuget_config_platform = match rt.platform() {
                        FlowPlatform::Windows => NugetInstallPlatform::Windows,
                        FlowPlatform::Linux(_) if crate::_util::running_in_wsl(rt) => {
                            if force_mono_nuget_exe_wsl2 {
                                NugetInstallPlatform::Linux
                            } else {
                                NugetInstallPlatform::Windows
                            }
                        }
                        FlowPlatform::Linux(_) => NugetInstallPlatform::Linux,
                        FlowPlatform::MacOs => NugetInstallPlatform::MacOs,
                        platform => anyhow::bail!("unsupported platform {platform}"),
                    };

                    rt.write_all(broadcast_nuget_config_platform, &nuget_config_platform);

                    Ok(())
                }
            });
        }

        match ctx.backend() {
            FlowBackend::Ado => Self::emit_ado(ctx, broadcast_nuget_tool_kind),
            FlowBackend::Local => {
                Self::emit_local(ctx, broadcast_nuget_tool_kind, force_mono_nuget_exe_wsl2)
            }
            FlowBackend::Github => {
                anyhow::bail!("nuget installation not yet implemented for the Github backend")
            }
        }
    }
}

impl Node {
    fn emit_ado(
        ctx: &mut NodeCtx<'_>,
        broadcast_nuget_tool_kind: Vec<WriteVar<PathBuf>>,
    ) -> anyhow::Result<()> {
        let nuget_tool_installed = ctx.reqv(crate::ado_task_nuget_tool_installer::Request);

        ctx.emit_rust_step("report nuget install", move |ctx| {
            nuget_tool_installed.claim(ctx);
            let broadcast_nuget_tool_kind = broadcast_nuget_tool_kind.claim(ctx);
            move |rt| {
                // trust that the ADO nuget install task works correctly
                rt.write_all(
                    broadcast_nuget_tool_kind,
                    &which::which(rt.platform().binary("nuget"))?,
                );

                Ok(())
            }
        });

        Ok(())
    }

    fn emit_local(
        ctx: &mut NodeCtx<'_>,
        broadcast_nuget_tool_kind: Vec<WriteVar<PathBuf>>,
        force_mono_nuget_exe_wsl2: bool,
    ) -> anyhow::Result<()> {
        if broadcast_nuget_tool_kind.is_empty() {
            return Ok(());
        }

        let install_dir = ctx
            .persistent_dir()
            .ok_or(anyhow::anyhow!("No persistent dir for nuget installation"))?;

        // let install_mono = if matches!(ctx.platform(), FlowPlatform::Linux(_)) {
        //     Some(ctx.reqv(|v| crate::install_dist_pkg::Request::Install {
        //         package_names: vec!["mono-devel".to_string()],
        //         done: v,
        //     }))
        // } else {
        //     None
        // }; // !!! TEMP !!! installing mono is breaking WSL2 interop!

        ctx.emit_rust_step("Install nuget", |ctx| {
            // install_mono.claim(ctx);
            let install_dir = install_dir.clone().claim(ctx);
            let broadcast_nuget_tool_kind = broadcast_nuget_tool_kind.claim(ctx);
            move |rt| {
                let install_dir = rt.read(install_dir);

                let nuget_exe_path = install_dir.join("nuget.exe");

                // download nuget if none was previously downloaded
                if !nuget_exe_path.exists() {
                    let nuget_install_latest_url =
                        "https://dist.nuget.org/win-x86-commandline/latest/nuget.exe";
                    flowey::shell_cmd!(
                        rt,
                        "curl --fail -o {nuget_exe_path} {nuget_install_latest_url}"
                    )
                    .run()?;
                }

                let write_mono_shim = |rt: &mut RustRuntimeServices<'_>| {
                    fs::write(
                        "./nuget-shim.sh",
                        format!("#!/bin/sh\nmono {}/nuget.exe \"$@\"", install_dir.display()),
                    )?;
                    flowey::shell_cmd!(rt, "chmod +x ./nuget-shim.sh").run()?;
                    anyhow::Ok(rt.sh.current_dir().join("nuget-shim.sh").absolute()?)
                };

                let nuget_exec_path = match rt.platform() {
                    FlowPlatform::Windows => nuget_exe_path,
                    FlowPlatform::Linux(_) if crate::_util::running_in_wsl(rt) => {
                        // allow reusing the windows config directory from wsl2, if available
                        {
                            let windows_userprofile =
                                flowey::shell_cmd!(rt, "cmd.exe /c echo %UserProfile%").read()?;

                            let windows_dot_nuget_path =
                                crate::_util::wslpath::win_to_linux(windows_userprofile)
                                    .join(".nuget");

                            let linux_dot_nuget_path =
                                dirs::home_dir().unwrap_or_default().join(".nuget");

                            // Only symlink if the user doesn't already have an
                            // existing .nuget folder / symlink
                            if windows_dot_nuget_path.exists()
                                && fs_err::symlink_metadata(&linux_dot_nuget_path).is_err()
                            {
                                flowey::shell_cmd!(
                                    rt,
                                    "ln -s {windows_dot_nuget_path} {linux_dot_nuget_path}"
                                )
                                .run()?;
                            }
                        }

                        if force_mono_nuget_exe_wsl2 {
                            write_mono_shim(rt)?
                        } else {
                            // rely on magical wsl2 interop

                            // WORKARDOUND: seems like on some folk's machines,
                            // nuget.exe will only work correctly when launched
                            // from a windows filesystem.
                            let windows_tempdir = crate::_util::wslpath::win_to_linux(
                                flowey::shell_cmd!(rt, "cmd.exe /c echo %Temp%").read()?,
                            );
                            let flowey_nuget = windows_tempdir.join("flowey_nuget.exe");
                            if !flowey_nuget.exists() {
                                fs_err::copy(nuget_exe_path, &flowey_nuget)?;
                            }
                            flowey::shell_cmd!(rt, "chmod +x {flowey_nuget}").run()?;
                            flowey_nuget
                        }
                    }
                    FlowPlatform::Linux(_) => write_mono_shim(rt)?,
                    platform => anyhow::bail!("unsupported platform {platform}"),
                };

                rt.write_all(broadcast_nuget_tool_kind, &nuget_exec_path);

                Ok(())
            }
        });

        Ok(())
    }
}
