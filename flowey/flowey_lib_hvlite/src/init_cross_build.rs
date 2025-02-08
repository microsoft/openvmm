// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Install dependencies and set environment variables for cross compiling

use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        pub target: target_lexicon::Triple,
        pub injected_env: WriteVar<BTreeMap<String, String>>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let host_platform = ctx.platform();
        let host_arch = ctx.arch();

        let native = |target: &target_lexicon::Triple| -> bool {
            let platform = match target.operating_system {
                target_lexicon::OperatingSystem::Windows => FlowPlatform::Windows,
                target_lexicon::OperatingSystem::Linux => {
                    FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu)
                }
                target_lexicon::OperatingSystem::Darwin => FlowPlatform::MacOs,
                _ => return false,
            };
            let arch = match target.architecture {
                target_lexicon::Architecture::X86_64 => FlowArch::X86_64,
                target_lexicon::Architecture::Aarch64(_) => FlowArch::Aarch64,
                _ => return false,
            };
            host_platform == platform && host_arch == arch
        };

        for Request {
            target,
            injected_env: injected_env_write,
        } in requests
        {
            let mut pre_build_deps = Vec::new();
            let mut injected_env = BTreeMap::new();

            if !native(&target) {
                match (ctx.platform(), target.operating_system) {
                    (FlowPlatform::Linux(_), target_lexicon::OperatingSystem::Linux) => {
                        let (gcc_pkg, bin) = match target.architecture {
                            target_lexicon::Architecture::Aarch64(_) => {
                                ("gcc-aarch64-linux-gnu", "aarch64-linux-gnu-gcc")
                            }
                            target_lexicon::Architecture::X86_64 => {
                                ("gcc-x86-64-linux-gnu", "x86_64-linux-gnu-gcc")
                            }
                            arch => anyhow::bail!("unsupported arch {arch}"),
                        };

                        // We use `gcc`'s linker for cross-compiling due to:
                        //
                        // * The special baremetal options are the same. These options
                        //   don't work for the LLVM linker,
                        // * The compiler team at Microsoft has stated that `rust-lld`
                        //   is not a production option,
                        // * The only Rust `aarch64` targets that produce
                        //   position-independent static ELF binaries with no std are
                        //   `aarch64-unknown-linux-*`.
                        pre_build_deps.push(ctx.reqv(|v| {
                            flowey_lib_common::install_dist_pkg::Request::Install {
                                package_names: vec![gcc_pkg.into()],
                                done: v,
                            }
                        }));

                        // when cross compiling for gnu linux, explicitly set the
                        // linker being used.
                        //
                        // Note: Don't do this for musl, since for that we use the
                        // openhcl linker set in the repo's `.cargo/config.toml`
                        // This isn't ideal because it means _any_ musl code (not just
                        // code running in VTL2) will use the openhcl-specific musl
                        if matches!(target.environment, target_lexicon::Environment::Gnu) {
                            injected_env.insert(
                                format!(
                                    "CARGO_TARGET_{}_LINKER",
                                    target.to_string().replace('-', "_").to_uppercase()
                                ),
                                bin.into(),
                            );
                        }
                    }
                    // Cross compiling for Windows relies on the appropriate
                    // Visual Studio Build Tools components being installed.
                    // The necessary libraries can be accessed from WSL,
                    // allowing for compilation of Windows applications from Linux.
                    // For now, just silently continue regardless.
                    // TODO: Detect (and potentially install) these dependencies
                    (FlowPlatform::Linux(_), target_lexicon::OperatingSystem::Windows) => {
                        let sh = xshell::Shell::new()?;
                        let workspace_root = xshell::cmd!(
                            sh,
                            "cargo locate-project --workspace --message-format=plain"
                        )
                        .read()
                        .map(PathBuf::from)
                        .map(|p| p.parent().unwrap().to_path_buf())
                        .map_err(|e| anyhow::anyhow!("Failed to get workspace root: {}", e))?;

                        let windows_cross_cl = "clang-cl-14";
                        let windows_cross_link = "lld-link-14";
                        let dll_tool = "llvm-dlltool-14";

                        let vswhere = xshell::cmd!(sh, "wslpath 'C:\\Program Files (x86)\\Microsoft Visual Studio\\Installer\\vswhere.exe'").read()?;
                        let vcvarsall = xshell::cmd!(sh, "{vswhere} -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -products '*' -latest -find 'VC\\Auxiliary\\Build\\vcvarsall.bat' -format value").read()?;
                        let vcvarsall_wsl = xshell::cmd!(sh, "wslpath {vcvarsall}").read()?;
                        let vcvarsall_path = xshell::cmd!(sh, "dirname {vcvarsall_wsl}").read()?;
                        let distro = std::env::var("WSL_DISTRO_NAME").unwrap();

                        sh.change_dir(vcvarsall_path);

                        let output = xshell::cmd!(sh, "cmd.exe /v:on /c .\\vcvarsall.bat x64 > nul && wsl -d {distro} echo '$INCLUDE' '^&^&' echo '$LIB'").env("WSLENV", "INCLUDE/l:LIB/l").read()?;
                        let converted = xshell::cmd!(sh, "tr ':' ';'").stdin(output).read()?;
                        let parts: Vec<&str> = converted.splitn(2, '\n').collect();
                        let include = parts
                            .first()
                            .ok_or_else(|| anyhow::anyhow!("Failed to split INCLUDE"))?;
                        let lib = parts
                            .first()
                            .ok_or_else(|| anyhow::anyhow!("Failed to split LIB"))?;

                        injected_env.insert("WINDOWS_CROSS_X86_64_LIB".into(), (*lib).into());
                        injected_env
                            .insert("WINDOWS_CROSS_X86_64_INCLUDE".into(), (*include).into());
                        injected_env.insert(
                            "CC_x86_64_pc_windows_msvc".into(),
                            workspace_root
                                .join("build_support/windows_cross/x86_64-clang-cl")
                                .to_string_lossy()
                                .into(),
                        );
                        injected_env.insert(
                            "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER".into(),
                            workspace_root
                                .join("build_support/windows_cross/x86_64-lld-link")
                                .to_string_lossy()
                                .into(),
                        );
                        injected_env
                            .insert("AR_x86_64_pc_windows_msvc".into(), "llvm-lib-14".into());
                        injected_env
                            .insert("RC_x86_64_pc_windows_msvc".into(), "llvm-rc-14".into());
                        injected_env.insert("DLLTOOL".into(), dll_tool.into());
                        injected_env.insert("WINDOWS_CROSS_CL".into(), windows_cross_cl.into());
                        injected_env.insert("WINDOWS_CROSS_LINK".into(), windows_cross_link.into());
                        injected_env
                            .insert("AR_aarch64_pc_windows_msvc".into(), "llvm-lib-14".into());
                        injected_env.insert(
                            "CC_aarch64_pc_windows_msvc".into(),
                            workspace_root
                                .join("build_support/windows_cross/aarch64-clang-cl")
                                .to_string_lossy()
                                .into(),
                        );
                        injected_env.insert(
                            "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER".into(),
                            workspace_root
                                .join("build_support/windows_cross/aarch64-lld-link")
                                .to_string_lossy()
                                .into(),
                        );
                        injected_env
                            .insert("WINDOWS_CROSS_AARCH64_INCLUDE".into(), (*include).into());
                        injected_env.insert("WINDOWS_CROSS_AARCH64_LIB".into(), (*lib).into());
                    }
                    (FlowPlatform::Windows, target_lexicon::OperatingSystem::Windows) => {}
                    (_, target_lexicon::OperatingSystem::None_) => {}
                    (_, target_lexicon::OperatingSystem::Uefi) => {}
                    (host_os, target_os) => {
                        anyhow::bail!("cannot cross compile for {target_os} on {host_os}")
                    }
                }
            }

            ctx.emit_rust_step("inject cross env", |ctx| {
                pre_build_deps.claim(ctx);
                let injected_env_write = injected_env_write.claim(ctx);
                move |rt| {
                    rt.write(injected_env_write, &injected_env);
                    Ok(())
                }
            });
        }

        Ok(())
    }
}
