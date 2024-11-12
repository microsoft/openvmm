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

        let installed_objcopy =
            ctx.reqv(
                |side_effect| flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec![match arch {
                        CommonArch::X86_64 => "binutils-x86-64-linux-gnu",
                        CommonArch::Aarch64 => "binutils-aarch64-linux-gnu",
                    }
                    .into()],
                    done: side_effect,
                },
            );

        ctx.emit_rust_step("split debug symbols", |ctx| {
            installed_objcopy.clone().claim(ctx);
            let in_bin = in_bin.claim(ctx);
            let out_bin = out_bin.claim(ctx);
            let out_dbg_info = out_dbg_info.claim(ctx);
            move |rt| {
                let in_bin = rt.read(in_bin);

                let sh = xshell::Shell::new()?;
                let output = sh.current_dir().join(in_bin.file_name().unwrap());
                let objcopy = format!("{}-linux-gnu-objcopy", arch.as_arch());
                xshell::cmd!(sh, "{objcopy} --only-keep-debug {in_bin} {output}.dbg").run()?;
                xshell::cmd!(
                    sh,
                    "{objcopy} --strip-all --keep-section=.build_info --add-gnu-debuglink={output}.dbg {in_bin} {output}"
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
