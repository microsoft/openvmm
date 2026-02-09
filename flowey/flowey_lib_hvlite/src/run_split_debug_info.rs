// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Splits debug info from a binary into a separate file using `objcopy`

use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub arch: CommonArch,
        pub in_bin: ReadVar<PathBuf>,
        pub out_bin: WriteVar<PathBuf>,
        pub out_dbg_info: WriteVar<PathBuf>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            arch,
            in_bin,
            out_bin,
            out_dbg_info,
        } = request;

        let host_arch = ctx.arch();
        let platform = ctx.platform();

        let (objcopy_pkg, objcopy_bin): (Option<&str>, &str) = match arch {
            CommonArch::X86_64 => match platform {
                FlowPlatform::Linux(linux_distribution) => match linux_distribution {
                    FlowPlatformLinuxDistro::Fedora => (
                        Some("binutils-x86_64-linux-gnu"),
                        "x86_64-linux-gnu-objcopy",
                    ),
                    FlowPlatformLinuxDistro::Ubuntu => (
                        Some("binutils-x86-64-linux-gnu"),
                        "x86_64-linux-gnu-objcopy",
                    ),
                    FlowPlatformLinuxDistro::Arch => {
                        match_arch!(host_arch, FlowArch::X86_64, (Some("binutils"), "objcopy"))
                    }
                    FlowPlatformLinuxDistro::Nix => (None, "x86_64-linux-gnu-objcopy"),
                    FlowPlatformLinuxDistro::Unknown => anyhow::bail!("Unknown Linux distribution"),
                },
                _ => anyhow::bail!("Unsupported platform"),
            },
            CommonArch::Aarch64 => {
                let pkg = match platform {
                    FlowPlatform::Linux(linux_distribution) => match linux_distribution {
                        FlowPlatformLinuxDistro::Fedora | FlowPlatformLinuxDistro::Ubuntu => {
                            Some("binutils-aarch64-linux-gnu")
                        }
                        FlowPlatformLinuxDistro::Arch => {
                            match_arch!(
                                host_arch,
                                FlowArch::X86_64,
                                Some("aarch64-linux-gnu-binutils")
                            )
                        }
                        FlowPlatformLinuxDistro::Nix => None,
                        FlowPlatformLinuxDistro::Unknown => {
                            anyhow::bail!("Unknown Linux distribution")
                        }
                    },
                    _ => anyhow::bail!("Unsupported platform"),
                };
                (pkg, "aarch64-linux-gnu-objcopy")
            }
        };

        let installed_objcopy = objcopy_pkg.map(|objcopy_pkg| {
            ctx.reqv(
                |side_effect| flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec![objcopy_pkg.into()],
                    done: side_effect,
                },
            )
        });

        ctx.emit_rust_step("split debug symbols", |ctx| {
            installed_objcopy.claim(ctx);
            let in_bin = in_bin.claim(ctx);
            let out_bin = out_bin.claim(ctx);
            let out_dbg_info = out_dbg_info.claim(ctx);
            move |rt| {
                let in_bin = rt.read(in_bin);

                let sh = xshell::Shell::new()?;
                let output = sh.current_dir().join(in_bin.file_name().unwrap());
                xshell::cmd!(sh, "{objcopy_bin} --only-keep-debug {in_bin} {output}.dbg").run()?;
                xshell::cmd!(
                    sh,
                    "{objcopy_bin} --strip-all --keep-section=.build_info --add-gnu-debuglink={output}.dbg {in_bin} {output}"
                )
                .run()?;

                let output = output.absolute()?;

                rt.write(out_bin, &output);
                rt.write(out_dbg_info, &output.with_extension("dbg"));

                Ok(())
            }
        });

        Ok(())
    }
}
