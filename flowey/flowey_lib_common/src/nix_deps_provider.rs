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
        /// Set the path to the repo containing shell.nix
        SetRepoPath(ReadVar<PathBuf>),
        GetOpenvmmDeps(OpenvmmDepsArch, WriteVar<PathBuf>),
        GetProtoc(WriteVar<PathBuf>),
        GetKernel(WriteVar<PathBuf>),
        GetUefiMuMsvm(WriteVar<PathBuf>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_nix::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut repo_path = None;
        let mut openvmm_deps_requests: BTreeMap<OpenvmmDepsArch, Vec<WriteVar<PathBuf>>> =
            BTreeMap::new();
        let mut protoc_requests = Vec::new();
        let mut openhcl_kernel_requests = Vec::new();
        let mut uefi_mu_msvm_requests = Vec::new();

        let nix_installed = ctx.reqv(crate::install_nix::Request::EnsureInstalled);

        // Parse all requests and group by type
        for req in requests {
            match req {
                Request::SetRepoPath(path) => {
                    same_across_all_reqs_backing_var("SetRepoPath", &mut repo_path, path)?;
                }
                Request::GetOpenvmmDeps(arch, var) => {
                    openvmm_deps_requests.entry(arch).or_default().push(var);
                }
                Request::GetProtoc(var) => protoc_requests.push(var),
                Request::GetKernel(var) => openhcl_kernel_requests.push(var),
                Request::GetUefiMuMsvm(var) => uefi_mu_msvm_requests.push(var),
            }
        }

        let repo_path = repo_path.context("Missing SetRepoPath request")?;

        // Only emit step if there are actual requests
        if openvmm_deps_requests.is_empty()
            && protoc_requests.is_empty()
            && openhcl_kernel_requests.is_empty()
            && uefi_mu_msvm_requests.is_empty()
        {
            return Ok(());
        }

        ctx.emit_rust_step("resolve nix dependency paths", |ctx| {
            nix_installed.claim(ctx);
            let repo_path = repo_path.claim(ctx);
            let openvmm_deps_requests = openvmm_deps_requests.claim(ctx);
            let protoc_requests = protoc_requests.claim(ctx);
            let openhcl_kernel_vmlinux_requests = openhcl_kernel_requests.claim(ctx);
            let uefi_mu_msvm_requests = uefi_mu_msvm_requests.claim(ctx);

            move |rt| {
                let repo_path = rt.read(repo_path);
                let sh = xshell::Shell::new()?;
                sh.change_dir(&repo_path);

                // Helper function to get environment variable from nix-shell
                let get_nix_env_var = |var_name: &str| -> anyhow::Result<String> {
                    let cmd_str = format!("echo ${}", var_name);
                    let output = xshell::cmd!(sh, "nix-shell --pure --run {cmd_str}")
                        .output()
                        .context(format!("Failed to run nix-shell to get {}", var_name))?;

                    if !output.status.success() {
                        anyhow::bail!(
                            "nix-shell command failed for {}: {}",
                            var_name,
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }

                    let value = String::from_utf8(output.stdout)?.trim().to_string();

                    if value.is_empty() {
                        anyhow::bail!(
                            "{} not set in nix-shell environment. Check shell.nix",
                            var_name
                        );
                    }

                    Ok(value)
                };

                // Read and write openvmm_deps to all requesting vars
                if !openvmm_deps_requests.is_empty() {
                    let openvmm_deps = get_nix_env_var("NIX_OPENVMM_DEPS")?;
                    let openvmm_deps_path = PathBuf::from(&openvmm_deps);

                    // Note: In Nix, the same package is used for both x64 and aarch64 at build time
                    for (arch, vars) in openvmm_deps_requests {
                        log::info!(
                            "Resolved Nix openvmm_deps for {:?}: {}",
                            arch,
                            openvmm_deps_path.display()
                        );
                        rt.write_all(vars, &openvmm_deps_path);
                    }
                }

                // Read and write protoc path if requested
                if !protoc_requests.is_empty() {
                    let protoc_path = get_nix_env_var("NIX_PROTOC_PATH")?;
                    let protoc_path = PathBuf::from(&protoc_path);

                    log::info!("Resolved Nix protoc: {}", protoc_path.display());
                    rt.write_all(protoc_requests, &protoc_path);
                }

                // Read and write openhcl_kernel vmlinux path if requested
                if !openhcl_kernel_vmlinux_requests.is_empty() {
                    let kernel_vmlinux = get_nix_env_var("NIX_OPENHCL_KERNEL")?;
                    let kernel_vmlinux_path = PathBuf::from(&kernel_vmlinux);

                    log::info!(
                        "Resolved Nix openhcl_kernel: {}",
                        kernel_vmlinux_path.display()
                    );
                    rt.write_all(openhcl_kernel_vmlinux_requests, &kernel_vmlinux_path);
                }

                // Read and write UEFI firmware path if requested
                if !uefi_mu_msvm_requests.is_empty() {
                    let uefi_mu_msvm = get_nix_env_var("NIX_UEFI_MU_MSVM")?;
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
