// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provide paths to dependencies managed by Nix.
//!
//! This node reads Nix environment variables and converts them to flowey's
//! ReadVar<PathBuf> system. It's used when building in a Nix environment
//! (USING_NIX=1) to pass Nix store paths to flowey jobs.

use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpenvmmDepsArch {
    X86_64,
    Aarch64,
}

flowey_request! {
    pub enum Request {
        /// Get the Nix store path for openvmm_deps
        GetOpenvmmDeps(OpenvmmDepsArch, WriteVar<PathBuf>),
        /// Get the Nix store path for protoc
        GetProtoc(WriteVar<PathBuf>),
        /// Get the Nix store path for openhcl_kernel vmlinux
        GetOpenhclKernelVmlinux(WriteVar<PathBuf>),
        /// Get the Nix store path for openhcl_kernel modules
        GetOpenhclKernelModules(WriteVar<PathBuf>),
        /// Get the Nix store path for UEFI firmware (MSVM.fd)
        GetUefiMuMsvm(WriteVar<PathBuf>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {
        // No dependencies needed - we just read environment variables
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut openvmm_deps_requests: BTreeMap<OpenvmmDepsArch, Vec<WriteVar<PathBuf>>> =
            BTreeMap::new();
        let mut protoc_requests = Vec::new();
        let mut openhcl_kernel_vmlinux_requests = Vec::new();
        let mut openhcl_kernel_modules_requests = Vec::new();
        let mut uefi_mu_msvm_requests = Vec::new();

        // Parse all requests and group by type
        for req in requests {
            match req {
                Request::GetOpenvmmDeps(arch, var) => {
                    openvmm_deps_requests.entry(arch).or_default().push(var);
                }
                Request::GetProtoc(var) => protoc_requests.push(var),
                Request::GetOpenhclKernelVmlinux(var) => openhcl_kernel_vmlinux_requests.push(var),
                Request::GetOpenhclKernelModules(var) => openhcl_kernel_modules_requests.push(var),
                Request::GetUefiMuMsvm(var) => uefi_mu_msvm_requests.push(var),
            }
        }

        // Only emit step if there are actual requests
        if openvmm_deps_requests.is_empty()
            && protoc_requests.is_empty()
            && openhcl_kernel_vmlinux_requests.is_empty()
            && openhcl_kernel_modules_requests.is_empty()
            && uefi_mu_msvm_requests.is_empty()
        {
            return Ok(());
        }

        ctx.emit_rust_step("resolve nix dependency paths", |ctx| {
            let openvmm_deps_requests = openvmm_deps_requests.claim(ctx);
            let protoc_requests = protoc_requests.claim(ctx);
            let openhcl_kernel_vmlinux_requests = openhcl_kernel_vmlinux_requests.claim(ctx);
            let openhcl_kernel_modules_requests = openhcl_kernel_modules_requests.claim(ctx);
            let uefi_mu_msvm_requests = uefi_mu_msvm_requests.claim(ctx);

            move |rt| {
                // Read Nix environment variables
                let openvmm_deps = std::env::var("OPENVMM_DEPS").context(
                    "OPENVMM_DEPS not set - are you running in a nix-shell environment?",
                )?;
                let openvmm_deps_path = PathBuf::from(&openvmm_deps);

                // Write openvmm_deps to all requesting vars
                // Note: In Nix, the same package is used for both x64 and aarch64 at build time
                for (arch, vars) in openvmm_deps_requests {
                    log::info!(
                        "Resolved Nix openvmm_deps for {:?}: {}",
                        arch,
                        openvmm_deps_path.display()
                    );
                    rt.write_all(vars, &openvmm_deps_path);
                }

                // Read and write protoc path if requested
                if !protoc_requests.is_empty() {
                    let protoc_path = std::env::var("NIX_PROTOC_PATH").context(
                        "NIX_PROTOC_PATH not set - ensure shell.nix exports this variable",
                    )?;
                    let protoc_path = PathBuf::from(&protoc_path);

                    log::info!("Resolved Nix protoc: {}", protoc_path.display());
                    rt.write_all(protoc_requests, &protoc_path);
                }

                // Read and write openhcl_kernel vmlinux path if requested
                if !openhcl_kernel_vmlinux_requests.is_empty() {
                    let kernel_vmlinux = std::env::var("NIX_OPENHCL_KERNEL_VMLINUX").context(
                        "NIX_OPENHCL_KERNEL_VMLINUX not set - ensure shell.nix exports this variable",
                    )?;
                    let kernel_vmlinux_path = PathBuf::from(&kernel_vmlinux);

                    log::info!(
                        "Resolved Nix openhcl_kernel vmlinux: {}",
                        kernel_vmlinux_path.display()
                    );
                    rt.write_all(openhcl_kernel_vmlinux_requests, &kernel_vmlinux_path);
                }

                // Read and write openhcl_kernel modules path if requested
                if !openhcl_kernel_modules_requests.is_empty() {
                    let kernel_modules = std::env::var("NIX_OPENHCL_KERNEL_MODULES").context(
                        "NIX_OPENHCL_KERNEL_MODULES not set - ensure shell.nix exports this variable",
                    )?;
                    let kernel_modules_path = PathBuf::from(&kernel_modules);

                    log::info!(
                        "Resolved Nix openhcl_kernel modules: {}",
                        kernel_modules_path.display()
                    );
                    rt.write_all(openhcl_kernel_modules_requests, &kernel_modules_path);
                }

                // Read and write UEFI firmware path if requested
                if !uefi_mu_msvm_requests.is_empty() {
                    let uefi_mu_msvm = std::env::var("NIX_UEFI_MU_MSVM").context(
                        "NIX_UEFI_MU_MSVM not set - ensure shell.nix exports this variable",
                    )?;
                    let uefi_mu_msvm_path = PathBuf::from(&uefi_mu_msvm);

                    log::info!(
                        "Resolved Nix UEFI firmware: {}",
                        uefi_mu_msvm_path.display()
                    );
                    rt.write_all(uefi_mu_msvm_requests, &uefi_mu_msvm_path);
                }

                Ok(())
            }
        });

        Ok(())
    }
}
