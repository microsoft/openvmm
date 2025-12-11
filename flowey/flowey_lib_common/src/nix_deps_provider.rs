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

        // Parse all requests and group by type
        for req in requests {
            match req {
                Request::GetOpenvmmDeps(arch, var) => {
                    openvmm_deps_requests.entry(arch).or_default().push(var);
                }
                Request::GetProtoc(var) => protoc_requests.push(var),
            }
        }

        // Only emit step if there are actual requests
        if openvmm_deps_requests.is_empty() && protoc_requests.is_empty() {
            return Ok(());
        }

        ctx.emit_rust_step("resolve nix dependency paths", |ctx| {
            let openvmm_deps_requests = openvmm_deps_requests.claim(ctx);
            let protoc_requests = protoc_requests.claim(ctx);

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

                Ok(())
            }
        });

        Ok(())
    }
}
