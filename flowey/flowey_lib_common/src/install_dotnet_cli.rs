// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Install or locate the `dotnet` CLI.
//!
//! On ADO, uses the `UseDotNet@2` task to install the .NET SDK.
//! On GitHub, expects `dotnet` to be pre-installed on the runner.
//! Locally, first checks if `dotnet` is already on PATH. If
//! `AutoInstall` is enabled and dotnet is not found, downloads and
//! runs the official `dotnet-install` script to install the SDK to a
//! persistent directory.

use flowey::node::prelude::*;

flowey_config! {
    /// Config for the install_dotnet_cli node.
    pub struct Config {
        /// Specify the .NET SDK *channel* to install (e.g. "8.0", "9.0").
        /// This is passed to `dotnet-install` as `--channel`, not as an exact
        /// SDK version.
        pub version: Option<String>,
        /// Automatically install the .NET SDK if not found on PATH.
        ///
        /// Must be set to true/false when running locally.
        pub auto_install: Option<bool>,
    }
}

flowey_request! {
    pub enum Request {
        /// Get the path to the `dotnet` binary.
        DotnetBin(WriteVar<PathBuf>),
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
        let mut broadcast_dotnet_bin = Vec::new();

        for req in requests {
            match req {
                Request::DotnetBin(outvar) => broadcast_dotnet_bin.push(outvar),
            }
        }

        if broadcast_dotnet_bin.is_empty() {
            return Ok(());
        }

        let version = config
            .version
            .ok_or(anyhow::anyhow!("missing config: version"))?;
        let auto_install = config.auto_install;

        // -- end of req processing -- //

        match ctx.backend() {
            FlowBackend::Ado => Self::emit_ado(ctx, broadcast_dotnet_bin, version),
            FlowBackend::Local => {
                let auto_install = auto_install
                    .ok_or(anyhow::anyhow!("Missing essential request: AutoInstall"))?;
                Self::emit_local(ctx, broadcast_dotnet_bin, version, auto_install)
            }
            FlowBackend::Github => Self::emit_github(ctx, broadcast_dotnet_bin),
        }
    }
}

impl Node {
    fn emit_ado(
        ctx: &mut NodeCtx<'_>,
        broadcast_dotnet_bin: Vec<WriteVar<PathBuf>>,
        version: String,
    ) -> anyhow::Result<()> {
        // UseDotNet@2 requires version in the format "major.minor.x" (e.g.
        // "8.0.x"), not just "8.0".
        let ado_version = if version.matches('.').count() < 2 {
            format!("{version}.x")
        } else {
            version
        };

        let (dotnet_installed, claim_dotnet_installed) = ctx.new_var::<SideEffect>();
        ctx.emit_ado_step("Install .NET SDK", move |ctx| {
            claim_dotnet_installed.claim(ctx);
            move |_| {
                format!(
                    r#"
                    - task: UseDotNet@2
                      inputs:
                        packageType: sdk
                        version: '{ado_version}'
                "#
                )
            }
        });

        ctx.emit_rust_step("report dotnet install", move |ctx| {
            dotnet_installed.claim(ctx);
            let broadcast_dotnet_bin = broadcast_dotnet_bin.claim(ctx);
            move |rt| {
                let dotnet_bin = which::which(rt.platform().binary("dotnet")).map_err(|_| {
                    anyhow::anyhow!("dotnet not found on PATH after UseDotNet task")
                })?;
                rt.write_all(broadcast_dotnet_bin, &dotnet_bin);
                Ok(())
            }
        });

        Ok(())
    }

    fn emit_local(
        ctx: &mut NodeCtx<'_>,
        broadcast_dotnet_bin: Vec<WriteVar<PathBuf>>,
        version: String,
        auto_install: bool,
    ) -> anyhow::Result<()> {
        if auto_install {
            let persistent_dir = ctx.persistent_dir();

            ctx.emit_rust_step("install dotnet", |ctx| {
                let persistent_dir = persistent_dir.clone().claim(ctx);
                let broadcast_dotnet_bin = broadcast_dotnet_bin.claim(ctx);
                move |rt| {
                    if let Some(existing_dotnet) = find_dotnet_on_path(rt) {
                        log::info!("found existing dotnet at {}", existing_dotnet.display());
                        rt.write_all(broadcast_dotnet_bin, &existing_dotnet);
                        return Ok(());
                    }

                    // Not on PATH — install via the official dotnet-install script
                    let install_dir = rt
                        .read(persistent_dir)
                        .ok_or(anyhow::anyhow!(
                            "dotnet is not on PATH and no persistent directory is configured. \
                             Please install the .NET SDK manually: \
                             https://dotnet.microsoft.com/download"
                        ))?
                        .join("dotnet");

                    let dotnet_bin_name = rt.platform().binary("dotnet");
                    let dotnet_bin_path = install_dir.join(&dotnet_bin_name);

                    if !dotnet_bin_path.exists() {
                        log::info!(
                            "dotnet not found on PATH or at {}, installing...",
                            dotnet_bin_path.display()
                        );

                        fs_err::create_dir_all(&install_dir)?;

                        match rt.platform() {
                            FlowPlatform::Windows => {
                                let install_script_url = "https://dot.net/v1/dotnet-install.ps1";
                                let install_script_path = install_dir
                                    .parent()
                                    .unwrap_or(&install_dir)
                                    .join("dotnet-install.ps1");

                                flowey::shell_cmd!(
                                    rt,
                                    "curl --fail -sSL -o {install_script_path} {install_script_url}"
                                )
                                .run()?;

                                flowey::shell_cmd!(
                                    rt,
                                    "powershell -ExecutionPolicy Bypass -File {install_script_path}
                                        -Channel {version}
                                        -InstallDir {install_dir}
                                        -NoPath
                                    "
                                )
                                .run()?;
                            }
                            FlowPlatform::Linux(_) | FlowPlatform::MacOs => {
                                let install_script_url = "https://dot.net/v1/dotnet-install.sh";
                                let install_script_path = install_dir
                                    .parent()
                                    .unwrap_or(&install_dir)
                                    .join("dotnet-install.sh");

                                flowey::shell_cmd!(
                                    rt,
                                    "curl --fail -sSL -o {install_script_path} {install_script_url}"
                                )
                                .run()?;

                                flowey::shell_cmd!(rt, "chmod +x {install_script_path}").run()?;

                                flowey::shell_cmd!(
                                    rt,
                                    "{install_script_path}
                                        --channel {version}
                                        --install-dir {install_dir}
                                        --no-path
                                    "
                                )
                                .run()?;
                            }
                            platform => {
                                anyhow::bail!("unsupported platform for dotnet install: {platform}")
                            }
                        }

                        if !dotnet_bin_path.exists() {
                            anyhow::bail!(
                                "dotnet installation completed but binary not found at {}",
                                dotnet_bin_path.display()
                            );
                        }
                    }

                    log::info!("using dotnet at {}", dotnet_bin_path.display());
                    rt.write_all(broadcast_dotnet_bin, &dotnet_bin_path);
                    Ok(())
                }
            });
        } else {
            // auto_install is false — just check PATH
            ctx.emit_rust_step("ensure dotnet is installed", |ctx| {
                let broadcast_dotnet_bin = broadcast_dotnet_bin.claim(ctx);
                move |rt| {
                    let dotnet_bin = find_dotnet_on_path(rt).ok_or_else(|| {
                        anyhow::anyhow!(
                            "dotnet is not installed. Please install the .NET SDK: \
                             https://dotnet.microsoft.com/download"
                        )
                    })?;
                    rt.write_all(broadcast_dotnet_bin, &dotnet_bin);
                    Ok(())
                }
            });
        }

        Ok(())
    }

    fn emit_github(
        ctx: &mut NodeCtx<'_>,
        broadcast_dotnet_bin: Vec<WriteVar<PathBuf>>,
    ) -> anyhow::Result<()> {
        // On GitHub Actions, dotnet is typically pre-installed.
        // Just locate it on PATH.
        ctx.emit_rust_step("resolve dotnet", |ctx| {
            let broadcast_dotnet_bin = broadcast_dotnet_bin.claim(ctx);
            move |rt| {
                let dotnet_bin = which::which(rt.platform().binary("dotnet")).map_err(|_| {
                    anyhow::anyhow!(
                        "dotnet not found on PATH. \
                         Add a `uses: actions/setup-dotnet` step to your workflow."
                    )
                })?;
                rt.write_all(broadcast_dotnet_bin, &dotnet_bin);
                Ok(())
            }
        });

        Ok(())
    }
}

/// Find `dotnet` on PATH, filtering out Windows `dotnet.exe` binaries
/// when running under WSL2 (since they cannot handle Linux paths).
fn find_dotnet_on_path(rt: &mut RustRuntimeServices<'_>) -> Option<PathBuf> {
    let path = which::which("dotnet").ok()?;
    if crate::_util::running_in_wsl(rt) {
        let is_windows_exe = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("exe"))
            .unwrap_or(false);
        if is_windows_exe {
            log::warn!(
                "ignoring Windows dotnet.exe at {} on WSL; \
                 a native Linux dotnet is required",
                path.display()
            );
            return None;
        }
    }
    Some(path)
}
