// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An amalgamated configuration node that streamlines the process of resolving
//! version configuration requests required by various dependencies in OpenVMM
//! pipelines.

use crate::common::CommonArch;
use crate::resolve_openhcl_kernel_package::OpenhclKernelPackageKind;
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
pub const DOTNET: &str = "8.0";
pub const FUZZ: &str = "0.12.0";
pub const GH_CLI: &str = "2.52.0";
pub const MDBOOK: &str = "0.4.40";
pub const MDBOOK_ADMONISH: &str = "1.18.0";
pub const MDBOOK_MERMAID: &str = "0.14.0";
pub const MU_MSVM: &str = "26.0.3";
pub const NEXTEST: &str = "0.9.101";
pub const NODEJS: &str = "24.x";
// N.B. Kernel version numbers for dev and stable branches are not directly
//      comparable. They originate from separate branches, and the fourth digit
//      increases with each release from the respective branch.
pub const OPENHCL_KERNEL_DEV_VERSION: &str = "6.18.0.2";
pub const OPENHCL_KERNEL_STABLE_VERSION: &str = "6.18.0.2";
pub const OPENVMM_DEPS: &str = "0.1.0-20260427.3";
pub const PROTOC: &str = "27.1";

flowey_request! {
    pub enum Request {
        /// Initialize the node, defaults to downloading everything
        Init,
        /// Override openvmm_deps with a local path for this architecture
        LocalOpenvmmDeps(CommonArch, ReadVar<PathBuf>),
        /// Override protoc with a local path
        LocalProtoc(ReadVar<PathBuf>),
        /// Override kernel with local paths (kernel binary, modules directory)
        LocalKernel {
            arch: CommonArch,
            kernel: ReadVar<PathBuf>,
            modules: ReadVar<PathBuf>,
        },
        /// Override UEFI mu_msvm with a local MSVM.fd path for this architecture
        LocalUefi(CommonArch, ReadVar<PathBuf>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::resolve_openhcl_kernel_package::Node>();
        ctx.import::<crate::resolve_openvmm_deps::Node>();
        ctx.import::<crate::download_uefi_mu_msvm::Node>();
        ctx.import::<crate::cfg_rustup_version::Node>();
        ctx.import::<flowey_lib_common::download_azcopy::Node>();
        ctx.import::<flowey_lib_common::download_cargo_fuzz::Node>();
        ctx.import::<flowey_lib_common::download_cargo_nextest::Node>();
        ctx.import::<flowey_lib_common::download_gh_cli::Node>();
        ctx.import::<flowey_lib_common::download_mdbook_admonish::Node>();
        ctx.import::<flowey_lib_common::download_mdbook_mermaid::Node>();
        ctx.import::<flowey_lib_common::download_mdbook::Node>();
        ctx.import::<flowey_lib_common::resolve_protoc::Node>();
        ctx.import::<flowey_lib_common::install_azure_cli::Node>();
        ctx.import::<flowey_lib_common::install_dotnet_cli::Node>();
        ctx.import::<flowey_lib_common::install_nodejs::Node>();
    }

    #[rustfmt::skip]
    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut local_openvmm_deps: BTreeMap<CommonArch, ReadVar<PathBuf>> = BTreeMap::new();
        let mut local_protoc: Option<ReadVar<PathBuf>> = None;
        let mut local_kernel: BTreeMap<CommonArch, (ReadVar<PathBuf>, ReadVar<PathBuf>)> = BTreeMap::new();
        let mut local_uefi: BTreeMap<CommonArch, ReadVar<PathBuf>> = BTreeMap::new();

        for req in requests {
            match req {
                Request::Init => {
                    // No-op, just ensures the node runs with defaults
                }
                Request::LocalOpenvmmDeps(arch, path) => {
                    if local_openvmm_deps.contains_key(&arch) {
                        anyhow::bail!(
                            "OpenvmmDepsPath for {:?} must not be specified multiple times",
                            arch
                        );
                    }
                    local_openvmm_deps.insert(arch, path);
                }
                Request::LocalProtoc(path) => {
                    if local_protoc.is_some() {
                        anyhow::bail!("ProtocPath must not be specified multiple times");
                    }
                    local_protoc = Some(path);
                }
                Request::LocalKernel { arch, kernel, modules } => {
                    if local_kernel.contains_key(&arch) {
                        anyhow::bail!(
                            "LocalKernel for {:?} must not be specified multiple times",
                            arch
                        );
                    }
                    local_kernel.insert(arch, (kernel, modules));
                }
                Request::LocalUefi(arch, path) => {
                    if local_uefi.contains_key(&arch) {
                        anyhow::bail!(
                            "LocalUefi for {:?} must not be specified multiple times",
                            arch
                        );
                    }
                    local_uefi.insert(arch, path);
                }
            }
        }

        // Track whether we have local paths for openvmm_deps and protoc
        let has_local_openvmm_deps = !local_openvmm_deps.is_empty();
        let has_local_protoc = local_protoc.is_some();
        let has_local_kernel = !local_kernel.is_empty();
        let has_local_uefi = !local_uefi.is_empty();

        // Set up local paths for openvmm_deps if provided
        if !local_openvmm_deps.is_empty() {
            let deps_local_paths = local_openvmm_deps
                .into_iter()
                .map(|(arch, path)| (arch, ConfigVar(path)))
                .collect();
            ctx.config(crate::resolve_openvmm_deps::Config {
                local_paths: deps_local_paths,
                ..Default::default()
            });
        }

        // Set up local path for protoc if provided
        if let Some(protoc_path) = local_protoc {
            ctx.config(flowey_lib_common::resolve_protoc::Config {
                local_path: Some(ConfigVar(protoc_path)),
                ..Default::default()
            });
        }

        // Set up local paths for kernel if provided
        if !local_kernel.is_empty() {
            let kernel_local_paths = local_kernel
                .into_iter()
                .map(|(arch, (kernel, modules))| {
                    (arch, (ConfigVar(kernel), ConfigVar(modules)))
                })
                .collect();
            ctx.config(crate::resolve_openhcl_kernel_package::Config {
                local_paths: kernel_local_paths,
                ..Default::default()
            });
        }

        // Set up local paths for UEFI if provided
        if !local_uefi.is_empty() {
            let uefi_local_paths = local_uefi
                .into_iter()
                .map(|(arch, path)| (arch, ConfigVar(path)))
                .collect();
            ctx.config(crate::download_uefi_mu_msvm::Config {
                local_paths: uefi_local_paths,
                ..Default::default()
            });
        }

        // Only set kernel versions if we don't have local paths
        // (versions are only needed for downloading)
        if !has_local_kernel {
            ctx.config(crate::resolve_openhcl_kernel_package::Config {
                versions: [
                    (OpenhclKernelPackageKind::Dev, OPENHCL_KERNEL_DEV_VERSION.into()),
                    (OpenhclKernelPackageKind::Main, OPENHCL_KERNEL_STABLE_VERSION.into()),
                    (OpenhclKernelPackageKind::Cvm, OPENHCL_KERNEL_STABLE_VERSION.into()),
                    (OpenhclKernelPackageKind::CvmDev, OPENHCL_KERNEL_DEV_VERSION.into()),
                ].into(),
                ..Default::default()
            });
        }
        if !has_local_openvmm_deps {
            ctx.config(crate::resolve_openvmm_deps::Config {
                version: Some(OPENVMM_DEPS.into()),
                ..Default::default()
            });
        }
        if !has_local_uefi {
            ctx.config(crate::download_uefi_mu_msvm::Config {
                version: Some(MU_MSVM.into()),
                ..Default::default()
            });
        }
        ctx.config(flowey_lib_common::download_azcopy::Config {
            version: Some(AZCOPY.into()),
        });
        ctx.config(flowey_lib_common::download_cargo_fuzz::Config {
            version: Some(FUZZ.into()),
        });
        ctx.config(flowey_lib_common::download_cargo_nextest::Config {
            version: Some(NEXTEST.into()),
        });
        ctx.config(flowey_lib_common::download_gh_cli::Config {
            version: Some(GH_CLI.into()),
        });
        ctx.config(flowey_lib_common::download_mdbook::Config {
            version: Some(MDBOOK.into()),
        });
        ctx.config(flowey_lib_common::download_mdbook_admonish::Config {
            version: Some(MDBOOK_ADMONISH.into()),
        });
        ctx.config(flowey_lib_common::download_mdbook_mermaid::Config {
            version: Some(MDBOOK_MERMAID.into()),
        });
        if !has_local_protoc {
            ctx.config(flowey_lib_common::resolve_protoc::Config {
                version: Some(PROTOC.into()),
                ..Default::default()
            });
        }
        ctx.config(flowey_lib_common::install_azure_cli::Config {
            version: Some(AZURE_CLI.into()),
            ..Default::default()
        });
        ctx.config(flowey_lib_common::install_dotnet_cli::Config {
            version: Some(DOTNET.into()),
            ..Default::default()
        });
        ctx.config(flowey_lib_common::install_nodejs::Config {
            version: Some(NODEJS.into()),
            ..Default::default()
        });
        ctx.req(crate::cfg_rustup_version::Request::Init);
        Ok(())
    }
}
