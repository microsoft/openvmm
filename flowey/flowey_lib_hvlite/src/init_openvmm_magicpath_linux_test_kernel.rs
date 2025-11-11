// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure the OpenVMM example linux kernel + initrd are extracted into the
//! correct "magic directory" set by the project-level `[env]` table in
//! `.cargo/config.toml`

use crate::resolve_openvmm_deps::OpenvmmDepsArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

new_flow_node!(struct Node);

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum OpenvmmLinuxTestKernelArch {
    Aarch64,
    X64,
}

flowey_request! {
    pub struct Request {
        pub arch: OpenvmmLinuxTestKernelArch,
        pub done: WriteVar<SideEffect>,
    }
}

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_openvmm_magicpath::Node>();
        ctx.import::<crate::resolve_openvmm_deps::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut kernel_arch: BTreeMap<_, Vec<_>> = BTreeMap::new();

        for Request { arch, done } in requests {
            kernel_arch.entry(arch).or_default().push(done)
        }

        let kernel_arch = kernel_arch;

        // -- end of req processing -- //

        let openvmm_magicpath = ctx.reqv(crate::cfg_openvmm_magicpath::Request);

        for (arch, out_vars) in kernel_arch {
            let openvmm_deps_arch = match arch {
                OpenvmmLinuxTestKernelArch::Aarch64 => OpenvmmDepsArch::Aarch64,
                OpenvmmLinuxTestKernelArch::X64 => OpenvmmDepsArch::X86_64,
            };
            let openvmm_linux_test_kernel = ctx.reqv(|v| {
                crate::resolve_openvmm_deps::Request::GetLinuxTestKernel(openvmm_deps_arch, v)
            });
            let openvmm_linux_test_initrd = ctx.reqv(|v| {
                crate::resolve_openvmm_deps::Request::GetLinuxTestInitrd(openvmm_deps_arch, v)
            });

            ctx.emit_rust_step(format!("extract {arch:?} sysroot.tar.gz"), |ctx| {
                let openvmm_linux_test_kernel = openvmm_linux_test_kernel.claim(ctx);
                let openvmm_linux_test_initrd = openvmm_linux_test_initrd.claim(ctx);
                let openvmm_magicpath = openvmm_magicpath.clone().claim(ctx);
                out_vars.claim(ctx);

                move |rt| {
                    let openvmm_linux_test_kernel = rt.read(openvmm_linux_test_kernel);
                    let openvmm_linux_test_initrd = rt.read(openvmm_linux_test_initrd);
                    let openvmm_magicpath = rt.read(openvmm_magicpath);

                    let test_kernel_path =
                        openvmm_magicpath
                            .join("underhill-deps-private")
                            .join(match arch {
                                OpenvmmLinuxTestKernelArch::Aarch64 => "aarch64",
                                OpenvmmLinuxTestKernelArch::X64 => "x64",
                            });
                    fs_err::create_dir_all(&test_kernel_path)?;
                    fs_err::copy(openvmm_linux_test_initrd, test_kernel_path.join("initrd"))?;
                    fs_err::copy(
                        openvmm_linux_test_kernel,
                        test_kernel_path.join(match arch {
                            OpenvmmLinuxTestKernelArch::Aarch64 => "Image",
                            OpenvmmLinuxTestKernelArch::X64 => "vmlinux",
                        }),
                    )?;

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
