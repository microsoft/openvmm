// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download `nuget.exe`

use flowey::node::prelude::*;
use std::fs;

flowey_request! {
    pub enum Request {
        /// Get the path to the `nuget.exe` binary (or a mono shim on Linux).
        NugetBin(WriteVar<PathBuf>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut broadcast_nuget_tool_kind = Vec::new();

        for req in requests {
            match req {
                Request::NugetBin(outvar) => broadcast_nuget_tool_kind.push(outvar),
            };
        }

        if broadcast_nuget_tool_kind.is_empty() {
            return Ok(());
        }

        // -- end of req processing -- //

        match ctx.backend() {
            FlowBackend::Ado => {
                anyhow::bail!("nuget.exe is not used on ADO. Use NuGetCommand@2 task directly.")
            }
            FlowBackend::Local => Self::emit_local(ctx, broadcast_nuget_tool_kind),
            FlowBackend::Github => {
                anyhow::bail!("nuget installation not yet implemented for the Github backend")
            }
        }
    }
}

impl Node {
    fn emit_local(
        ctx: &mut NodeCtx<'_>,
        broadcast_nuget_tool_kind: Vec<WriteVar<PathBuf>>,
    ) -> anyhow::Result<()> {
        let install_dir = ctx
            .persistent_dir()
            .ok_or(anyhow::anyhow!("No persistent dir for nuget installation"))?;

        ctx.emit_rust_step("Install nuget", |ctx| {
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
                                crate::_util::wslpath::win_to_linux(rt, windows_userprofile)
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

                        // rely on magical wsl2 interop

                        // WORKAROUND: seems like on some folk's machines,
                        // nuget.exe will only work correctly when launched
                        // from a windows filesystem.
                        let windows_tempdir = crate::_util::wslpath::win_to_linux(
                            rt,
                            flowey::shell_cmd!(rt, "cmd.exe /c echo %Temp%").read()?,
                        );
                        let flowey_nuget = windows_tempdir.join("flowey_nuget.exe");
                        if !flowey_nuget.exists() {
                            fs_err::copy(nuget_exe_path, &flowey_nuget)?;
                        }
                        flowey::shell_cmd!(rt, "chmod +x {flowey_nuget}").run()?;
                        flowey_nuget
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
