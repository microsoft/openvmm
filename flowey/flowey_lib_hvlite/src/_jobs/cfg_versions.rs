// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An amalgamated configuration node that streamlines the process of resolving
//! version configuration requests required by various dependencies in OpenVMM
//! pipelines.

use crate::download_openhcl_kernel_package::OpenhclKernelPackageKind;
use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;

// FUTURE: instead of hard-coding these values in-code, we might want to make
// our own nuget-esque `packages.config` file, that we can read at runtime to
// resolve all Version requests.
//
// This would require nodes that currently accept a `Version(String)` to accept
// a `Version(ReadVar<String>)`, but that shouldn't be a serious blocker.
pub const AZCOPY: &str = "10.27.1-20241113";
pub const AZURE_CLI: &str = "2.56.0";
pub const FUZZ: &str = "0.12.0";
pub const GH_CLI: &str = "2.52.0";
pub const MDBOOK: &str = "0.4.40";
pub const MDBOOK_ADMONISH: &str = "1.18.0";
pub const MDBOOK_MERMAID: &str = "0.14.0";
pub const RUSTUP_TOOLCHAIN: &str = "1.91.1";
pub const MU_MSVM: &str = "25.1.9";
pub const NEXTEST: &str = "0.9.101";
pub const NODEJS: &str = "18.x";
// N.B. Kernel version numbers for dev and stable branches are not directly
//      comparable. They originate from separate branches, and the fourth digit
//      increases with each release from the respective branch.
pub const OPENHCL_KERNEL_DEV_VERSION: &str = "6.12.52.2";
pub const OPENHCL_KERNEL_STABLE_VERSION: &str = "6.12.52.2";
pub const OPENVMM_DEPS: &str = "0.1.0-20250403.3";
pub const PROTOC: &str = "27.1";

flowey_request! {
    pub enum Request {
        Download,
        Local(CommonArch, ReadVar<PathBuf>, ReadVar<PathBuf>),
        NixEnvironment,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::download_openhcl_kernel_package::Node>();
        ctx.import::<crate::download_openhcl_kernel_package::Node>();
        ctx.import::<crate::resolve_openvmm_deps::Node>();
        ctx.import::<crate::download_uefi_mu_msvm::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
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
        ctx.import::<flowey_lib_common::nix_deps_provider::Node>();
    }

    #[rustfmt::skip]
    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        use std::collections::BTreeMap;

        let mut has_local_requests = false;
        let mut has_nix_requests = false;
        let mut local_openvmm_deps: BTreeMap<CommonArch, ReadVar<PathBuf>> = BTreeMap::new();
        let mut local_protoc: Option<ReadVar<PathBuf>> = None;
        let mut local_kernel: Option<ReadVar<PathBuf>> = None;
        let mut local_uefi: Option<ReadVar<PathBuf>> = None;

        for req in requests {
            match req {
                Request::Download => {
                    // Download requests are always allowed and coexist with Local/Nix requests
                }
                Request::Local(arch, path, protoc_path) => {
                    has_local_requests = true;

                    // Check that for every arch that shows up, the path is always the same
                    if let Some(existing_path) = local_openvmm_deps.get(&arch) {
                        if !existing_path.eq(&path) {
                            anyhow::bail!(
                                "OpenvmmDepsPath for {:?} must be consistent across requests",
                                arch
                            );
                        }
                    } else {
                        local_openvmm_deps.insert(arch, path);
                    }

                    same_across_all_reqs_backing_var("ProtocPath", &mut local_protoc, protoc_path)?;
                }
                Request::NixEnvironment => {
                    has_nix_requests = true;
                }
            }
        }

        // If NixEnvironment was requested, get paths from nix_deps_provider
        if has_nix_requests {
            // Get the repo path so nix_deps_provider can access shell.nix
            let repo_path = ctx.reqv(|v| crate::git_checkout_openvmm_repo::Request::GetRepoDir(
                crate::git_checkout_openvmm_repo::req::GetRepoDir(v)
            ));
            ctx.req(flowey_lib_common::nix_deps_provider::Request::SetRepoPath(repo_path));

            let nix_openvmm_deps_x64 = ctx.reqv(|v| flowey_lib_common::nix_deps_provider::Request::GetOpenvmmDeps(
                flowey_lib_common::nix_deps_provider::OpenvmmDepsArch::X86_64,
                v,
            ));
            local_openvmm_deps.insert(CommonArch::X86_64, nix_openvmm_deps_x64);
            local_protoc = Some(ctx.reqv(flowey_lib_common::nix_deps_provider::Request::GetProtoc));
            local_kernel = Some(ctx.reqv(flowey_lib_common::nix_deps_provider::Request::GetKernel));
            local_uefi = Some(ctx.reqv(flowey_lib_common::nix_deps_provider::Request::GetUefiMuMsvm));
        }

        // Track whether we have local paths for openvmm_deps and protoc
        let has_local_openvmm_deps = !local_openvmm_deps.is_empty();
        let has_local_protoc = local_protoc.is_some();
        let has_local_kernel = local_kernel.is_some();
        let has_local_uefi = local_uefi.is_some();

        // If we have local requests, protoc must be provided
        if has_local_requests && local_protoc.is_none() {
            anyhow::bail!("Local mode requires protoc path to be specified");
        }

        // Set up local paths for openvmm_deps if provided
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

        // Set up local path for protoc if provided
        if let Some(protoc_path) = local_protoc {
            ctx.req(flowey_lib_common::download_protoc::Request::LocalPath(
                protoc_path,
            ));
        }

        if let Some(kernel_path) = local_kernel {
            ctx.req(crate::download_openhcl_kernel_package::Request::LocalPath(kernel_path));
        }

        if let Some(uefi_path) = local_uefi {
            ctx.req(crate::download_uefi_mu_msvm::Request::LocalPath(uefi_path));
        }

        // Set up version requests for everything
        if !has_local_kernel {
            ctx.req(crate::download_openhcl_kernel_package::Request::Version(OpenhclKernelPackageKind::Dev, OPENHCL_KERNEL_DEV_VERSION.into()));
            ctx.req(crate::download_openhcl_kernel_package::Request::Version(OpenhclKernelPackageKind::Main, OPENHCL_KERNEL_STABLE_VERSION.into()));
            ctx.req(crate::download_openhcl_kernel_package::Request::Version(OpenhclKernelPackageKind::Cvm, OPENHCL_KERNEL_STABLE_VERSION.into()));
            ctx.req(crate::download_openhcl_kernel_package::Request::Version(OpenhclKernelPackageKind::CvmDev, OPENHCL_KERNEL_DEV_VERSION.into()));
        }

        if !has_local_openvmm_deps {
            ctx.req(crate::resolve_openvmm_deps::Request::Version(OPENVMM_DEPS.into()));
        }

        if !has_local_uefi {
            ctx.req(crate::download_uefi_mu_msvm::Request::Version(MU_MSVM.into()));
        }

        ctx.req(flowey_lib_common::download_azcopy::Request::Version(AZCOPY.into()));
        ctx.req(flowey_lib_common::download_cargo_fuzz::Request::Version(FUZZ.into()));
        ctx.req(flowey_lib_common::download_cargo_nextest::Request::Version(NEXTEST.into()));
        ctx.req(flowey_lib_common::download_gh_cli::Request::Version(GH_CLI.into()));
        ctx.req(flowey_lib_common::download_mdbook::Request::Version(MDBOOK.into()));
        ctx.req(flowey_lib_common::download_mdbook_admonish::Request::Version(MDBOOK_ADMONISH.into()));
        ctx.req(flowey_lib_common::download_mdbook_mermaid::Request::Version(MDBOOK_MERMAID.into()));
        if !has_local_protoc {
            ctx.req(flowey_lib_common::download_protoc::Request::Version(PROTOC.into()));
        }
        ctx.req(flowey_lib_common::install_azure_cli::Request::Version(AZURE_CLI.into()));
        ctx.req(flowey_lib_common::install_nodejs::Request::Version(NODEJS.into()));
        if !matches!(ctx.backend(), FlowBackend::Ado) {
            ctx.req(flowey_lib_common::install_rust::Request::Version(RUSTUP_TOOLCHAIN.into()));
        }
        Ok(())
    }
}
