// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local-only job that supports the `cargo xflowey run-igvm` CLI

use crate::build_openvmm;
use crate::build_openvmm::OpenvmmBuildParams;
use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<build_openvmm::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Self::Request { done } = request;

        ctx.emit_rust_step("run openhcl", |ctx| {
            done.claim(ctx);
            |rt| {
                let sh = xshell::Shell::new()?;

                let windows_cross_cl = "clang-cl-14";
                let windows_cross_link = "lld-link-14";
                let dll_tool = "llvm-dlltool-14";

                let vswhere = xshell::cmd!(sh, "wslpath 'C:\\Program Files (x86)\\Microsoft Visual Studio\\Installer\\vswhere.exe'").read()?;
                println!("vswhere: {}", vswhere);
                let vcvarsall = xshell::cmd!(sh, "{vswhere} -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -products '*' -latest -find 'VC\\Auxiliary\\Build\\vcvarsall.bat' -format value").read()?;
                println!("vcvarsall: {}", vcvarsall);
                let vcvarsall_wsl = xshell::cmd!(sh, "wslpath {vcvarsall}").read()?;
                println!("vcvarsallwsl: {}", vcvarsall_wsl);
                let vcvarsall_path = xshell::cmd!(sh, "dirname {vcvarsall_wsl}").read()?;
                println!("vcvarsall_path: {}", vcvarsall_path);
                let distro = std::env::var("WSL_DISTRO_NAME").unwrap();

                let cwd = sh.current_dir();
                sh.change_dir(vcvarsall_path);

                let output = xshell::cmd!(sh, "cmd.exe /v:on /c .\\vcvarsall.bat x64 > nul && wsl -d {distro} echo '$INCLUDE' '^&^&' echo '$LIB'").env("WSLENV", "INCLUDE/l:LIB/l").read()?;
                let converted = xshell::cmd!(sh, "tr ':' ';'").stdin(output).read()?;
                let parts: Vec<&str> = converted.splitn(2, '\n').collect();
                let include = parts.get(0).ok_or_else(|| anyhow::anyhow!("Failed to split INCLUDE"))?;
                let lib = parts.get(1).ok_or_else(|| anyhow::anyhow!("Failed to split LIB"))?;

                sh.change_dir(cwd);

                let quoted_lib_entries: String = lib
                    .split(';')
                    .map(|entry| format!("\"{}\"", entry))
                    .collect::<Vec<String>>()
                    .join(";");

                let quoted_include_entries: String = include
                    .split(';')
                    .map(|entry| format!("\"{}\"", entry))
                    .collect::<Vec<String>>()
                    .join(";");

                xshell::cmd!(sh, "printenv").run()?;

                xshell::cmd!(sh, "cargo build --target x86_64-pc-windows-msvc")
                .env("WINDOWS_CROSS_X86_64_LIB", quoted_lib_entries.clone())
                .env("WINDOWS_CROSS_X86_64_INCLUDE", quoted_include_entries)
                .env("CC_x86_64_pc_windows_msvc", "/home/justuscamp/openvmm/build_support/windows_cross/x86_64-clang-cl")
                .env("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER", "/home/justuscamp/openvmm/build_support/windows_cross/x86_64-lld-link")
                .env("AR_x86_64_pc_windows_msvc", quoted_lib_entries)
                .env("RC_x86_64_pc_windows_msvc", "llvm-rc-14")
                .env("DLLTOOL", dll_tool)
                .env("WINDOWS_CROSS_CL", windows_cross_cl)
                .env("WINDOWS_CROSS_LINK", windows_cross_link)
                .run()?;


                /*
                fs_err::copy(, "/mnt/c/tmp/openvmm.exe")?;
                let openhcl_path = r#"\\wsl.localhost\Ubuntu\home\justuscamp\openvmm\flowey-out\artifacts\build-igvm\debug\x64\openhcl-x64.bin"#;
                xshell::cmd!(sh, "/mnt/c/tmp/openvmm.exe --hv --vtl2 --igvm {openhcl_path} --com3 console -m 4GB --com1 none --vmbus-com1-serial term=wt --vmbus-com2-serial term=wt --net uh:consomme --vmbus-redirect --no-alias-map").run()?;
                */
                Ok(())
            }
        });

        Ok(())
    }
}
