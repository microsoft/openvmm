// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An amalgamated configuration node that streamlines the process of resolving
//! version configuration requests required by various dependencies in OpenVMM
//! pipelines.

use crate::download_openhcl_kernel_package::OpenhclKernelPackageKind;
use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

// FUTURE: instead of hard-coding these values in-code, we might want to make
// our own nuget-esque `packages.config` file, that we can read at runtime to
// resolve all Version requests.
//
// This would require nodes that currently accept a `Version(String)` to accept
// a `Version(ReadVar<String>)`, but that shouldn't be a serious blocker.
pub const AZCOPY: &str = "10.27.1";
pub const AZURE_CLI: &str = "2.56.0";
pub const FUZZ: &str = "0.12.0";
pub const GH_CLI: &str = "2.52.0";
pub const MDBOOK: &str = "0.4.40";
pub const MDBOOK_ADMONISH: &str = "1.18.0";
pub const MDBOOK_MERMAID: &str = "0.14.0";
pub const RUSTUP_TOOLCHAIN: &str = "1.91.1";
pub const MU_MSVM: &str = "25.1.9";
pub const NEXTEST: &str = "0.9.101";
pub const NODEJS: &str = "24.x";
// N.B. Kernel version numbers for dev and stable branches are not directly
//      comparable. They originate from separate branches, and the fourth digit
//      increases with each release from the respective branch.
pub const OPENHCL_KERNEL_DEV_VERSION: &str = "6.12.52.4";
pub const OPENHCL_KERNEL_STABLE_VERSION: &str = "6.12.52.4";
pub const OPENVMM_DEPS: &str = "0.1.0-20250403.3";
pub const PROTOC: &str = "27.1";

flowey_request! {
    pub enum Request {
        Download,
        Local(CommonArch, PathBuf),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::download_openhcl_kernel_package::Node>();
        ctx.import::<crate::resolve_openvmm_deps::Node>();
        ctx.import::<crate::download_uefi_mu_msvm::Node>();
        ctx.import::<flowey_lib_common::download_azcopy::Node>();
        ctx.import::<flowey_lib_common::download_cargo_fuzz::Node>();
        ctx.import::<flowey_lib_common::download_cargo_nextest::Node>();
        ctx.import::<flowey_lib_common::download_gh_cli::Node>();
        ctx.import::<flowey_lib_common::download_mdbook_admonish::Node>();
        ctx.import::<flowey_lib_common::download_mdbook_mermaid::Node>();
        ctx.import::<flowey_lib_common::download_mdbook::Node>();
        ctx.import::<flowey_lib_common::download_protoc::Node>();
        ctx.import::<flowey_lib_common::install_azure_cli::Node>();
        ctx.import::<flowey_lib_common::install_nodejs::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
    }

    #[rustfmt::skip]
    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut has_download_requests = false;
        let mut has_local_requests = false;
        let mut local_openvmm_deps: BTreeMap<CommonArch, PathBuf> = BTreeMap::new();

        for req in requests {
            match req {
                Request::Download => {
                    has_download_requests = true;
                }
                Request::Local(arch, path) => {
                    has_local_requests = true;

                    // Check that for every arch that shows up, the path is always the same
                    if let Some(existing_path) = local_openvmm_deps.get(&arch) {
                        if existing_path != &path {
                            anyhow::bail!(
                                "OpenvmmDepsPath for {:?} must be consistent across requests",
                                arch
                            );
                        }
                    } else {
                        local_openvmm_deps.insert(arch, path);
                    }
                }
            }
        }

        if has_download_requests && has_local_requests {
            anyhow::bail!("cannot mix Download and Local requests");
        }

        if has_local_requests {
            for (arch, path) in local_openvmm_deps {
                let openvmm_deps_arch = match arch {
                    CommonArch::X86_64 => crate::resolve_openvmm_deps::OpenvmmDepsArch::X86_64,
                    CommonArch::Aarch64 => crate::resolve_openvmm_deps::OpenvmmDepsArch::Aarch64,
                };

                ctx.req(crate::resolve_openvmm_deps::Request::LocalPath(
                    openvmm_deps_arch,
                    path,
                ));
            }

            anyhow::bail!("using local dependencies not yet fully implemented");
        }

        ctx.req(crate::download_openhcl_kernel_package::Request::Version(OpenhclKernelPackageKind::Dev, OPENHCL_KERNEL_DEV_VERSION.into()));
        ctx.req(crate::download_openhcl_kernel_package::Request::Version(OpenhclKernelPackageKind::Main, OPENHCL_KERNEL_STABLE_VERSION.into()));
        ctx.req(crate::download_openhcl_kernel_package::Request::Version(OpenhclKernelPackageKind::Cvm, OPENHCL_KERNEL_STABLE_VERSION.into()));
        ctx.req(crate::download_openhcl_kernel_package::Request::Version(OpenhclKernelPackageKind::CvmDev, OPENHCL_KERNEL_DEV_VERSION.into()));
        ctx.req(crate::resolve_openvmm_deps::Request::Version(OPENVMM_DEPS.into()));
        ctx.req(crate::download_uefi_mu_msvm::Request::Version(MU_MSVM.into()));
        ctx.req(flowey_lib_common::download_azcopy::Request::Version(AZCOPY.into()));
        ctx.req(flowey_lib_common::download_cargo_fuzz::Request::Version(FUZZ.into()));
        ctx.req(flowey_lib_common::download_cargo_nextest::Request::Version(NEXTEST.into()));
        ctx.req(flowey_lib_common::download_gh_cli::Request::Version(GH_CLI.into()));
        ctx.req(flowey_lib_common::download_mdbook::Request::Version(MDBOOK.into()));
        ctx.req(flowey_lib_common::download_mdbook_admonish::Request::Version(MDBOOK_ADMONISH.into()));
        ctx.req(flowey_lib_common::download_mdbook_mermaid::Request::Version(MDBOOK_MERMAID.into()));
        ctx.req(flowey_lib_common::download_protoc::Request::Version(PROTOC.into()));
        ctx.req(flowey_lib_common::install_azure_cli::Request::Version(AZURE_CLI.into()));
        ctx.req(flowey_lib_common::install_nodejs::Request::Version(NODEJS.into()));
        if !matches!(ctx.backend(), FlowBackend::Ado) {
            ctx.req(flowey_lib_common::install_rust::Request::Version(RUSTUP_TOOLCHAIN.into()));
        }
        Ok(())
    }
}
