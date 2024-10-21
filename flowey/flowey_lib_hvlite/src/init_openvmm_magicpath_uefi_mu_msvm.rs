// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure the mu_msvm MSVM.fd file is copied into the "magic directory" to
//! automatically work in the context of the OpenVMM repo.
//!
//! Eventually, this will only be required for interactive use-cases (i.e: to
//! support openvmm's `X86_64_OPENVMM_UEFI_FIRMWARE` local-only `[env]` var).
//!
//! Work is ongoing to root out remaining hard-codes to this magic path by
//! various other bits of repo tooling (notably: petri's `known_paths`
//! resolver).

use crate::download_uefi_mu_msvm::MuMsvmArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        pub arch: MuMsvmArch,
        pub done: WriteVar<SideEffect>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_openvmm_magicpath::Node>();
        ctx.import::<crate::download_uefi_mu_msvm::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut reqs: BTreeMap<MuMsvmArch, Vec<WriteVar<SideEffect>>> = BTreeMap::new();
        for Request { arch, done } in requests {
            reqs.entry(arch).or_default().push(done);
        }

        // -- end of req processing -- //

        let packages = reqs
            .into_iter()
            .map(|(arch, dones)| {
                (
                    arch,
                    (
                        ctx.reqv(|v| crate::download_uefi_mu_msvm::Request::GetMsvmFd {
                            arch,
                            msvm_fd: v,
                        }),
                        dones,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();

        let openvmm_magicpath = ctx.reqv(crate::cfg_openvmm_magicpath::Request);

        ctx.emit_rust_step("move MSVM.fd into its magic folder", move |ctx| {
            let packages = packages.claim(ctx);
            let openvmm_magicpath = openvmm_magicpath.claim(ctx);
            move |rt| {
                let openvmm_magicpath = rt.read(openvmm_magicpath);
                for (arch, (msvm_fd, _dones)) in packages {
                    let msvm_fd = rt.read(msvm_fd);
                    let dst_folder = openvmm_magicpath
                        .join(format!(
                            "hyperv.uefi.mscoreuefi.{}.RELEASE",
                            match arch {
                                MuMsvmArch::Aarch64 => "AARCH64",
                                MuMsvmArch::X86_64 => "x64",
                            }
                        ))
                        .join(format!(
                            "Msvm{}",
                            match arch {
                                MuMsvmArch::Aarch64 => "AARCH64",
                                MuMsvmArch::X86_64 => "X64",
                            }
                        ))
                        .join("RELEASE_VS2022/FV");
                    let dst = dst_folder.join("MSVM.fd");

                    if msvm_fd.absolute()? != dst.absolute()? {
                        fs_err::create_dir_all(&dst_folder)?;
                        fs_err::copy(msvm_fd, dst)?;
                    }
                }
                Ok(())
            }
        });

        Ok(())
    }
}
