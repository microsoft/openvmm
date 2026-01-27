// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An amalgamated configuration node that streamlines the process of resolving
//! version configuration requests required by various dependencies in OpenVMM
//! pipelines.

use crate::resolve_openhcl_kernel_package::OpenhclKernelPackageKind;
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
        /// Initialize the node, defaults to downloading everything
        Init,
        /// Override openvmm_deps with a local path for this architecture
        LocalOpenvmmDeps(CommonArch, PathBuf),
        /// Override protoc with a local path
        LocalProtoc(PathBuf),
        /// Override kernel with local paths (kernel binary, modules directory)
        LocalKernel {
            arch: CommonArch,
            kernel: PathBuf,
            modules: PathBuf,
        },
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::resolve_openhcl_kernel_package::Node>();
        ctx.import::<crate::resolve_openvmm_deps::Node>();
        ctx.import::<crate::download_uefi_mu_msvm::Node>();
        ctx.import::<crate::_jobs::cfg_rustup_version::Node>();
        ctx.import::<flowey_lib_common::download_azcopy::Node>();
        ctx.import::<flowey_lib_common::download_cargo_fuzz::Node>();
        ctx.import::<flowey_lib_common::download_cargo_nextest::Node>();
        ctx.import::<flowey_lib_common::download_gh_cli::Node>();
        ctx.import::<flowey_lib_common::download_mdbook_admonish::Node>();
        ctx.import::<flowey_lib_common::download_mdbook_mermaid::Node>();
        ctx.import::<flowey_lib_common::download_mdbook::Node>();
        ctx.import::<flowey_lib_common::resolve_protoc::Node>();
        ctx.import::<flowey_lib_common::install_azure_cli::Node>();
        ctx.import::<flowey_lib_common::install_nodejs::Node>();
    }

    #[rustfmt::skip]
    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut local_openvmm_deps: BTreeMap<CommonArch, PathBuf> = BTreeMap::new();
        let mut local_protoc: Option<PathBuf> = None;
        let mut local_kernel: BTreeMap<CommonArch, (PathBuf, PathBuf)> = BTreeMap::new();

        for req in requests {
            match req {
                Request::Init => {
                    // No-op, just ensures the node runs with defaults
                }
                Request::LocalOpenvmmDeps(arch, path) => {
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
                Request::LocalProtoc(path) => {
                    same_across_all_reqs("ProtocPath", &mut local_protoc, path)?;
                }
                Request::LocalKernel { arch, kernel, modules } => {
                    let paths = (kernel, modules);
                    if let Some(existing) = local_kernel.get(&arch) {
                        if existing != &paths {
                            anyhow::bail!(
                                "LocalKernel for {:?} must be consistent across requests",
                                arch
                            );
                        }
                    } else {
                        local_kernel.insert(arch, paths);
                    }
                }
            }
        }

        // Track whether we have local paths for openvmm_deps and protoc
        let has_local_openvmm_deps = !local_openvmm_deps.is_empty();
        let has_local_protoc = local_protoc.is_some();
        let has_local_kernel = !local_kernel.is_empty();

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
            ctx.req(flowey_lib_common::resolve_protoc::Request::LocalPath(
                protoc_path,
            ));
        }

        // Set up local paths for kernel if provided
        for (arch, (kernel, modules)) in local_kernel {
            let kernel_arch = match arch {
                CommonArch::X86_64 => crate::resolve_openhcl_kernel_package::OpenhclKernelPackageArch::X86_64,
                CommonArch::Aarch64 => crate::resolve_openhcl_kernel_package::OpenhclKernelPackageArch::Aarch64,
            };
            ctx.req(crate::resolve_openhcl_kernel_package::Request::SetLocal {
                arch: kernel_arch,
                kernel,
                modules,
            });
        }

        // Only set kernel versions if we don't have local paths
        // (versions are only needed for downloading)
        if !has_local_kernel {
            ctx.req(crate::resolve_openhcl_kernel_package::Request::SetVersion(OpenhclKernelPackageKind::Dev, OPENHCL_KERNEL_DEV_VERSION.into()));
            ctx.req(crate::resolve_openhcl_kernel_package::Request::SetVersion(OpenhclKernelPackageKind::Main, OPENHCL_KERNEL_STABLE_VERSION.into()));
            ctx.req(crate::resolve_openhcl_kernel_package::Request::SetVersion(OpenhclKernelPackageKind::Cvm, OPENHCL_KERNEL_STABLE_VERSION.into()));
            ctx.req(crate::resolve_openhcl_kernel_package::Request::SetVersion(OpenhclKernelPackageKind::CvmDev, OPENHCL_KERNEL_DEV_VERSION.into()));
        }
        if !has_local_openvmm_deps {
            ctx.req(crate::resolve_openvmm_deps::Request::Version(OPENVMM_DEPS.into()));
        }
        ctx.req(crate::download_uefi_mu_msvm::Request::Version(MU_MSVM.into()));
        ctx.req(flowey_lib_common::download_azcopy::Request::Version(AZCOPY.into()));
        ctx.req(flowey_lib_common::download_cargo_fuzz::Request::Version(FUZZ.into()));
        ctx.req(flowey_lib_common::download_cargo_nextest::Request::Version(NEXTEST.into()));
        ctx.req(flowey_lib_common::download_gh_cli::Request::Version(GH_CLI.into()));
        ctx.req(flowey_lib_common::download_mdbook::Request::Version(MDBOOK.into()));
        ctx.req(flowey_lib_common::download_mdbook_admonish::Request::Version(MDBOOK_ADMONISH.into()));
        ctx.req(flowey_lib_common::download_mdbook_mermaid::Request::Version(MDBOOK_MERMAID.into()));
        if !has_local_protoc {
            ctx.req(flowey_lib_common::resolve_protoc::Request::Version(PROTOC.into()));
        }
        ctx.req(flowey_lib_common::install_azure_cli::Request::Version(AZURE_CLI.into()));
        ctx.req(flowey_lib_common::install_nodejs::Request::Version(NODEJS.into()));
        ctx.req(crate::_jobs::cfg_rustup_version::Request::Init);
        Ok(())
    }
}
