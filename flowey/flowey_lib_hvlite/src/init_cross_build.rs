// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Install dependencies and set environment variables for cross compiling

use flowey::node::prelude::*;
use std::collections::BTreeMap;
use target_lexicon::Architecture;

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
            // Check if the target matches the host platform, treat Linux distros as equivalent
            let os_matches = matches!(
                (host_platform, target.operating_system),
                (
                    FlowPlatform::Linux(_),
                    target_lexicon::OperatingSystem::Linux
                ) | (
                    FlowPlatform::Windows,
                    target_lexicon::OperatingSystem::Windows
                ) | (
                    FlowPlatform::MacOs,
                    target_lexicon::OperatingSystem::Darwin(_)
                )
            );

            let arch_matches = match target.architecture {
                Architecture::X86_64 => host_arch == FlowArch::X86_64,
                Architecture::Aarch64(_) => host_arch == FlowArch::Aarch64,
                _ => false,
            };

            os_matches && arch_matches
        };

        for Request {
            target,
            injected_env: injected_env_write,
        } in requests
        {
            let mut pre_build_deps = Vec::new();
            let mut injected_env = BTreeMap::new();

            if !native(&target) {
                let platform = ctx.platform();

                match (platform, target.operating_system) {
                    (FlowPlatform::Linux(_), target_lexicon::OperatingSystem::Linux) => {
                        let (gcc_pkg, bin): (Option<&str>, String) = match target.architecture {
                            Architecture::X86_64 => match platform {
                                FlowPlatform::Linux(linux_distribution) => {
                                    let pkg = match linux_distribution {
                                        FlowPlatformLinuxDistro::Fedora => {
                                            Some("gcc-x86_64-linux-gnu")
                                        }
                                        FlowPlatformLinuxDistro::Ubuntu => {
                                            Some("gcc-x86-64-linux-gnu")
                                        }
                                        FlowPlatformLinuxDistro::Arch => {
                                            match_arch!(host_arch, FlowArch::X86_64, Some("gcc"))
                                        }
                                        FlowPlatformLinuxDistro::Nix => None,
                                        FlowPlatformLinuxDistro::Unknown => {
                                            anyhow::bail!("Unknown Linux distribution")
                                        }
                                    };
                                    (pkg, "x86_64-linux-gnu-gcc".to_string())
                                }
                                _ => anyhow::bail!("Unsupported platform"),
                            },
                            Architecture::Aarch64(_) => match platform {
                                FlowPlatform::Linux(linux_distribution) => {
                                    let pkg = match linux_distribution {
                                        FlowPlatformLinuxDistro::Fedora
                                        | FlowPlatformLinuxDistro::Ubuntu => {
                                            Some("gcc-aarch64-linux-gnu")
                                        }
                                        FlowPlatformLinuxDistro::Arch => match_arch!(
                                            host_arch,
                                            FlowArch::X86_64,
                                            Some("aarch64-linux-gnu-gcc")
                                        ),
                                        FlowPlatformLinuxDistro::Nix => None,
                                        FlowPlatformLinuxDistro::Unknown => {
                                            anyhow::bail!("Unknown Linux distribution")
                                        }
                                    };
                                    (pkg, "aarch64-linux-gnu-gcc".to_string())
                                }
                                _ => anyhow::bail!("Unsupported platform"),
                            },
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
                        //
                        // Skip package installation for Nix (shell.nix provides cross-compilers)
                        if let Some(gcc_pkg) = gcc_pkg {
                            pre_build_deps.push(ctx.reqv(|v| {
                                flowey_lib_common::install_dist_pkg::Request::Install {
                                    package_names: vec![gcc_pkg.into()],
                                    done: v,
                                }
                            }));
                        }

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
                                bin,
                            );
                        }
                    }
                    // Cross compiling for Windows relies on the appropriate
                    // Visual Studio Build Tools components being installed.
                    // The necessary libraries can be accessed from WSL,
                    // allowing for compilation of Windows applications from Linux.
                    // For now, just silently continue regardless.
                    // TODO: Detect (and potentially install) these dependencies
                    (FlowPlatform::Linux(_), target_lexicon::OperatingSystem::Windows) => {}
                    (FlowPlatform::Windows, target_lexicon::OperatingSystem::Windows) => {}
                    (_, target_lexicon::OperatingSystem::None_) => {}
                    (_, target_lexicon::OperatingSystem::Uefi) => {}
                    (host_os, target_os) => {
                        anyhow::bail!("cannot cross compile for {target_os} on {host_os}")
                    }
                }
            }

            ctx.emit_minor_rust_step("inject cross env", |ctx| {
                pre_build_deps.claim(ctx);
                let injected_env_write = injected_env_write.claim(ctx);
                move |rt| {
                    rt.write(injected_env_write, &injected_env);
                }
            });
        }

        Ok(())
    }
}
