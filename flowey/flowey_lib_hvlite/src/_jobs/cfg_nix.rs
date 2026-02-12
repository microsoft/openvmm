// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A configuration node that resolves dependency paths from the Nix environment.
//!
//! When running inside a `nix-shell`, all external dependencies (protoc,
//! openvmm-deps, kernel packages, UEFI firmware) are provided by Nix at
//! well-known paths exposed via environment variables. This node reads those
//! paths at runtime, writes them to a [`NixConfig`] struct, and issues the
//! corresponding [`cfg_versions::Request`] overrides so that downstream nodes
//! use the Nix-provided artifacts instead of downloading them.

use crate::_jobs::cfg_versions;
use crate::resolve_openhcl_kernel_package::OpenhclKernelPackageKind;
use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;

/// Resolved dependency paths from the Nix environment.
#[derive(Serialize, Deserialize)]
pub struct NixConfig {
    pub openvmm_deps: PathBuf,
    pub protoc: PathBuf,
    pub kernel: PathBuf,
    pub kernel_modules: PathBuf,
    pub uefi: PathBuf,
}

flowey_request! {
    pub struct Params {
        pub arch: CommonArch,
        pub kernel_kind: OpenhclKernelPackageKind,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<cfg_versions::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params { arch, kernel_kind } = request;

        let arch_suffix = match arch {
            CommonArch::X86_64 => "X64",
            CommonArch::Aarch64 => "AARCH64",
        };
        let kernel_suffix = match kernel_kind {
            OpenhclKernelPackageKind::Main => "",
            OpenhclKernelPackageKind::Cvm => "_CVM",
            OpenhclKernelPackageKind::Dev => "_DEV",
            OpenhclKernelPackageKind::CvmDev => "_CVM_DEV",
        };
        let openvmm_deps_env = format!("OPENVMM_DEPS_{arch_suffix}");
        let kernel_env = format!("NIX_KERNEL_{arch_suffix}{kernel_suffix}");
        let uefi_env = format!("NIX_UEFI_{arch_suffix}");
        let kernel_file = match arch {
            CommonArch::X86_64 => "vmlinux",
            CommonArch::Aarch64 => "Image",
        };

        let (nix_config_read, nix_config_write) = ctx.new_var::<NixConfig>();

        ctx.emit_rust_step("resolve nix dependency paths", |ctx| {
            let nix_config_write = nix_config_write.claim(ctx);
            move |rt| {
                let openvmm_deps =
                    PathBuf::from(flowey::shell_cmd!(rt, "printenv {openvmm_deps_env}").read()?);
                let protoc = PathBuf::from(flowey::shell_cmd!(rt, "printenv NIX_PROTOC").read()?);
                let kernel_pkg =
                    PathBuf::from(flowey::shell_cmd!(rt, "printenv {kernel_env}").read()?);
                let uefi = PathBuf::from(flowey::shell_cmd!(rt, "printenv {uefi_env}").read()?);

                let kernel = kernel_pkg.join(kernel_file);
                let kernel_modules = kernel_pkg.join("modules");

                log::info!("resolved nix paths:");
                log::info!("  openvmm_deps: {}", openvmm_deps.display());
                log::info!("  protoc:       {}", protoc.display());
                log::info!("  kernel:       {}", kernel.display());
                log::info!("  modules:      {}", kernel_modules.display());
                log::info!("  uefi:         {}", uefi.display());

                rt.write(
                    nix_config_write,
                    &NixConfig {
                        openvmm_deps,
                        protoc,
                        kernel,
                        kernel_modules,
                        uefi,
                    },
                );

                Ok(())
            }
        });

        let openvmm_deps = nix_config_read.map(ctx, |cfg| cfg.openvmm_deps);
        let protoc = nix_config_read.map(ctx, |cfg| cfg.protoc);
        let kernel = nix_config_read.map(ctx, |cfg| cfg.kernel);
        let kernel_modules = nix_config_read.map(ctx, |cfg| cfg.kernel_modules);
        let uefi = nix_config_read.map(ctx, |cfg| cfg.uefi);

        // Set the cfg_versions overrides to use all "local" paths
        ctx.req(cfg_versions::Request::Init);
        ctx.req(cfg_versions::Request::LocalOpenvmmDeps(arch, openvmm_deps));
        ctx.req(cfg_versions::Request::LocalProtoc(protoc));
        ctx.req(cfg_versions::Request::LocalKernel {
            arch,
            kernel,
            modules: kernel_modules,
        });
        ctx.req(cfg_versions::Request::LocalUefi(arch, uefi));

        Ok(())
    }
}
