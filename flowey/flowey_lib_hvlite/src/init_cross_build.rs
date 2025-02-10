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
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
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
            let mut openvmm_repo_path = None;
            let mut runtime_env_for_cross = None;

            if !native(&target)
                && matches!(
                    (ctx.platform(), target.operating_system),
                    (
                        FlowPlatform::Linux(_),
                        target_lexicon::OperatingSystem::Windows
                    )
                )
            {
                runtime_env_for_cross = Some(ctx.new_var());
                openvmm_repo_path =
                    Some(ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir));
            }

            let (env_write_var, env_read_var) = if let Some(var) = runtime_env_for_cross {
                (Some(var.1), Some(var.0))
            } else {
                (None, None)
            };

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
                        if let Some(write_var) = env_write_var {
                            pre_build_deps.push(ctx.emit_rust_step(
                            "get windows cross build environment variables",
                            |ctx| {
                                let runtime_env = write_var.claim(ctx);
                                let openvmm_repo_path = openvmm_repo_path.unwrap().claim(ctx);
                                |rt| {
                                    let mut env = BTreeMap::new();
                                    let openvmm_repo_path = rt.read(openvmm_repo_path);

                                    let sh = xshell::Shell::new()?;
                                    let env_vars = xshell::cmd!(
                                        sh,
                                        "{openvmm_repo_path}/build_support/setup_windows_cross.sh --print-only"
                                    )
                                    .read()?;

                                    for line in env_vars.lines() {
                                        if let Some((key, value)) = line.split_once('=') {
                                            env.insert(key.to_string(), value.to_string());
                                        }
                                    }

                                    rt.write(runtime_env, &env);

                                    Ok(())
                                }
                            },
                        ));
                        }
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
                let runtime_env = env_read_var.map(|var| var.claim(ctx));

                move |rt| {
                    if let Some(runtime_env) = runtime_env {
                        let runtime_env = rt.read(runtime_env);
                        for (k, v) in runtime_env {
                            injected_env.insert(k, v);
                        }
                    }

                    rt.write(injected_env_write, &injected_env);
                    Ok(())
                }
            });
        }

        Ok(())
    }
}
