// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::download_lxutil::LxutilArch;
use crate::download_uefi_mu_msvm::MuMsvmArch;
use crate::init_openvmm_magicpath_linux_test_kernel::OpenvmmLinuxTestKernelArch;
use crate::init_openvmm_magicpath_openhcl_sysroot::OpenvmmSysrootArch;
use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request{
        pub arch: CommonArch,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::init_openvmm_magicpath_linux_test_kernel::Node>();
        ctx.import::<crate::init_openvmm_magicpath_lxutil::Node>();
        ctx.import::<crate::init_openvmm_magicpath_openhcl_sysroot::Node>();
        ctx.import::<crate::init_openvmm_magicpath_protoc::Node>();
        ctx.import::<crate::init_openvmm_magicpath_uefi_mu_msvm::Node>();
        ctx.import::<flowey_lib_common::install_lcov::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { arch, done } = request;

        let mut deps = vec![ctx.reqv(crate::init_openvmm_magicpath_protoc::Request)];

        // Install lcov for fuzzing coverage reports
        deps.push(ctx.reqv(|done| flowey_lib_common::install_lcov::Request::EnsureInstalled(done)));

        match arch {
            CommonArch::X86_64 => {
                if matches!(ctx.platform(), FlowPlatform::Linux(_)) {
                    deps.extend_from_slice(&[ctx
                        .reqv(|v| crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                            arch: OpenvmmSysrootArch::X64,
                            path: v,
                        })
                        .into_side_effect()]);
                }
                deps.extend_from_slice(&[
                    ctx.reqv(|done| crate::init_openvmm_magicpath_lxutil::Request {
                        arch: LxutilArch::X86_64,
                        done,
                    }),
                    ctx.reqv(|done| crate::init_openvmm_magicpath_uefi_mu_msvm::Request {
                        arch: MuMsvmArch::X86_64,
                        done,
                    }),
                    ctx.reqv(
                        |done| crate::init_openvmm_magicpath_linux_test_kernel::Request {
                            arch: OpenvmmLinuxTestKernelArch::X64,
                            done,
                        },
                    ),
                ]);
            }
            CommonArch::Aarch64 => {
                if matches!(ctx.platform(), FlowPlatform::Linux(_)) {
                    deps.extend_from_slice(&[ctx
                        .reqv(|v| crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                            arch: OpenvmmSysrootArch::Aarch64,
                            path: v,
                        })
                        .into_side_effect()]);
                }
                deps.extend_from_slice(&[
                    ctx.reqv(|done| crate::init_openvmm_magicpath_lxutil::Request {
                        arch: LxutilArch::Aarch64,
                        done,
                    }),
                    ctx.reqv(|done| crate::init_openvmm_magicpath_uefi_mu_msvm::Request {
                        arch: MuMsvmArch::Aarch64,
                        done,
                    }),
                    ctx.reqv(
                        |done| crate::init_openvmm_magicpath_linux_test_kernel::Request {
                            arch: OpenvmmLinuxTestKernelArch::Aarch64,
                            done,
                        },
                    ),
                ]);
            }
        }

        ctx.emit_side_effect_step(deps, [done]);

        Ok(())
    }
}
