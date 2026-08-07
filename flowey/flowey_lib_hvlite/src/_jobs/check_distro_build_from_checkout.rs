// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assemble the OpenVMM source archive and run the distribution-build gate.

use crate::assemble_openvmm_source_release::SourceReleaseOutput;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::assemble_openvmm_source_release::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::_jobs::check_distro_build::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { done } = request;

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let resolved = ctx.emit_rust_stepv("resolve source archive identity", |ctx| {
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            move |rt| {
                let assets = std::env::current_dir()?.join("openvmm-source-release");
                let path = rt.read(openvmm_repo_path);
                rt.sh.change_dir(path);

                Ok((
                    crate::assemble_openvmm_source_release::resolve_identity(rt)?,
                    assets,
                ))
            }
        });
        let identity = resolved.clone().map(ctx, |(identity, _)| identity);
        let output_dir = resolved.clone().map(ctx, |(_, assets)| assets);
        let assembled = ctx.reqv(|done| crate::assemble_openvmm_source_release::Request {
            identity,
            output_dir,
            done,
        });
        let release = resolved
            .depending_on(ctx, &assembled)
            .map(ctx, |(_, assets)| SourceReleaseOutput { assets });

        ctx.req(crate::_jobs::check_distro_build::Request { release, done });

        Ok(())
    }
}
