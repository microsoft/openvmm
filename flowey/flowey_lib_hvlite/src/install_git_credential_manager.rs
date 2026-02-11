// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Globally install the git credential manager

use flowey::node::prelude::*;
use flowey_lib_common::_util::wslpath;

flowey_request! {
    pub enum Request {
        /// Automatically configure the user's global config manager
        AutoConfigure,
        /// WSL2 will use linux git credential manager if true, windows version if false.
        UseNativeLinuxOnWsl2,
        /// Ensure that git was configured
        EnsureConfigured(WriteVar<SideEffect>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(dep: &mut ImportCtx<'_>) {
        dep.import::<flowey_lib_common::install_git::Node>();
        dep.import::<flowey_lib_common::check_needs_relaunch::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        if !matches!(ctx.backend(), FlowBackend::Local) {
            anyhow::bail!("only supported on the local backend at this time");
        }

        let mut use_native_linux_on_wsl2 = None;
        let mut auto_configure = None;
        let mut ensure_configured = Vec::new();

        for req in requests {
            match req {
                Request::AutoConfigure => {
                    same_across_all_reqs("AutoConfigure", &mut auto_configure, true)?
                }
                Request::UseNativeLinuxOnWsl2 => same_across_all_reqs(
                    "UseNativeLinuxOnWsl2",
                    &mut use_native_linux_on_wsl2,
                    true,
                )?,
                Request::EnsureConfigured(v) => ensure_configured.push(v),
            }
        }

        let use_native_linux_on_wsl2 = use_native_linux_on_wsl2.unwrap_or(false);
        let auto_configure = auto_configure.unwrap_or(false);

        // -- end of req processing -- //

        let git_ensure_installed =
            ctx.reqv(flowey_lib_common::install_git::Request::EnsureInstalled);

        let (read_env, write_env) = ctx.new_var();
        ctx.req(flowey_lib_common::check_needs_relaunch::Params {
            check: read_env,
            done: ensure_configured,
        });

        ctx.emit_rust_step("configure git credential manager", move |ctx| {
            git_ensure_installed.clone().claim(ctx);
            let write_env = write_env.claim(ctx);

            move |rt: &mut RustRuntimeServices<'_>| {
                let mut env_to_write = None;
                let existing_credman = flowey::shell_cmd!(rt, "git config --global credential.helper").ignore_status().read()?;
                log::info!("existing credentials helper: {existing_credman}");

                if !existing_credman.is_empty() {
                    if existing_credman.contains("git-credential-manager")
                        || existing_credman.contains("credential-manager-core")
                        || existing_credman.contains("manager")
                    {
                        log::info!("existing credentials helper matches a known-good credential helper.");
                        rt.write(write_env, &None);
                        return Ok(())
                    } else {
                        log::warn!("existing credentials helper isn't any of the known-good credential helpers.");
                        log::warn!("assume the user knows what they're doing?");
                        rt.write(write_env, &None);
                        return Ok(())
                    }
                }

                // give the user an option to maunally install the cred manager
                if existing_credman.is_empty() && !auto_configure {
                    log::info!("Could not detect an existing Git Credential helper.");
                    log::info!("Press <y> to automatically configure an appropriate --global configuration helper, or any other key to abort the run.");
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    if input.trim() != "y" {
                        anyhow::bail!("aborting...")
                    }
                }

                if flowey_lib_common::_util::running_in_wsl(rt) && !use_native_linux_on_wsl2 {
                    let windows_user_profile_path_windows = flowey::shell_cmd!(rt, "cmd.exe /c echo %UserProfile%").read().map_err(|_| anyhow::anyhow!("Unable to run cmd.exe, please restart WSL by running `wsl --shutdown` in powershell and try again."))?;
                    let windows_user_profile_path = wslpath::win_to_linux(windows_user_profile_path_windows);
                    let gcm_path_opt_1 = windows_user_profile_path.join("AppData/Local/Programs/Git Credential Manager/git-credential-manager.exe");
                    let gcm_path_opt_2 = wslpath::win_to_linux(r#"C:\Program Files\Git\mingw64\bin\git-credential-manager.exe"#);
                    let gcm_path_opt_3 = wslpath::win_to_linux(r#"C:\Program Files\Git\mingw64\libexec\git-core\git-credential-manager.exe"#);
                    let gcm_path_opt_4 = wslpath::win_to_linux(r#"C:\Program Files (x86)\Git Credential Manager\git-credential-manager.exe"#);

                    let gcm_path = if rt.sh.path_exists(&gcm_path_opt_1) {
                        &gcm_path_opt_1
                    } else if rt.sh.path_exists(&gcm_path_opt_2) {
                        &gcm_path_opt_2
                    } else if rt.sh.path_exists(&gcm_path_opt_3) {
                        &gcm_path_opt_3
                    } else if rt.sh.path_exists(&gcm_path_opt_4) {
                        &gcm_path_opt_4
                    } else {
                        anyhow::bail!("Git Credential Manager not found, please install it manually.");
                    };

                    if gcm_path == &gcm_path_opt_1 || gcm_path == &gcm_path_opt_4 {
                        let mut wslenv = rt.sh.var("WSLENV")?;
                        if !wslenv.contains("GIT_EXEC_PATH/wp") {
                            log::info!("Standalone Git Credential Manager has been detected.");
                            log::info!("Please run the following from an administrator command prompt to configure it:");
                            log::info!("SETX WSLENV %WSLENV%:GIT_EXEC_PATH/wp");
                            log::info!("This command shares `GIT_EXEC_PATH`, an environment variable which determines where Git looks for its sub-programs,");
                            log::info!(" with WSL processes spawned from Win32 and vice versa. `/wp` are flags specifying how `GIT_EXEC_PATH` gets translated.");
                            log::info!("Please refer to <https://devblogs.microsoft.com/commandline/share-environment-vars-between-wsl-and-windows/>");
                            log::info!("and <https://git-scm.com/book/en/v2/Git-Internals-Environment-Variables> for more details.");

                            let do_config = if !auto_configure {
                                wslenv.push_str(":GIT_EXEC_PATH/wp");

                                log::info!("Please press <y> to automatically configure it, or any other key to continue after manual configuration.");
                                let mut input = String::new();
                                std::io::stdin().read_line(&mut input)?;
                                input.trim() == "y"
                            } else {
                                true
                            };

                            if do_config {
                                flowey::shell_cmd!(rt, "setx.exe WSLENV {wslenv}").run()?;
                            }

                            env_to_write = Some(flowey_lib_common::check_needs_relaunch::BinOrEnv::Env("WSLENV".to_string(), "GIT_EXEC_PATH/wp".to_string()));
                        }
                    }

                    // Have to do this weird string business due to requiring the escaped space character in Program\ Files
                    let gcm_path_str = gcm_path.to_str().expect("Invalid git credential manager path").to_string().replace(' ', "\\ ");
                    flowey::shell_cmd!(rt, "git config --global credential.helper {gcm_path_str}").run()?;
                    flowey::shell_cmd!(rt, "git config --global credential.https://dev.azure.com.useHttpPath true").run()?;
                } else if matches!(rt.platform(), FlowPlatform::Windows) {
                    flowey::shell_cmd!(rt, "git config --global credential.helper manager").run()?;
                } else {
                    anyhow::bail!("git credential manager configuration only supported for windows/wsl2 at this time")
                }

                rt.write(write_env, &env_to_write);
                Ok(())
            }
        });

        Ok(())
    }
}
