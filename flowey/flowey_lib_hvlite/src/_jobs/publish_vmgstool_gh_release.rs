// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publishes a GitHub release for VmgsTool

use crate::build_vmgstool::VmgstoolOutput;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        pub vmgstools: BTreeMap<String, ReadVar<VmgstoolOutput>>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::publish_gh_release::Node>();
        ctx.import::<flowey_lib_common::get_cargo_crate_version::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { vmgstools, done } = request;

        let files = ctx.emit_rust_stepv("enumerate vmgstool release files", |ctx| {
            let vmgstools = vmgstools
                .into_iter()
                .map(|(t, v)| (t, v.claim(ctx)))
                .collect::<BTreeMap<_, _>>();
            move |rt| {
                let mut files = Vec::new();
                for (target, vmgstool) in vmgstools {
                    let vmgstool = rt.read(vmgstool);
                    match vmgstool {
                        VmgstoolOutput::LinuxBin { bin, dbg } => {
                            files.push((bin, Some(format!("vmgstool-{target}"))));
                            files.push((dbg, Some(format!("vmgstool-{target}.dbg"))));
                        }
                        VmgstoolOutput::WindowsBin { exe, pdb } => {
                            files.push((exe, Some(format!("vmgstool-{target}.exe"))));
                            files.push((pdb, Some(format!("vmgstool-{target}.pdb"))));
                        }
                    }
                }
                Ok(files)
            }
        });

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let vmgstool_path =
            openvmm_repo_path.map(ctx, |p| p.join("vm").join("vmgs").join("vmgstool"));
        let version = ctx.reqv(|v| flowey_lib_common::get_cargo_crate_version::Request {
            path: vmgstool_path,
            version: v,
        });
        let version = version.map(ctx, |v| v.unwrap_or("0.1.0".into()));
        let tag = version.map(ctx, |v| format!("vmgstool-v{v}"));
        let title = version.map(ctx, |v| format!("VmgsTool v{v}"));

        ctx.req(flowey_lib_common::publish_gh_release::Request(
            flowey_lib_common::publish_gh_release::GhReleaseParams {
                repo_owner: "microsoft".into(),
                repo_name: "openvmm".into(),
                tag,
                title,
                files,
                draft: true,
                done,
            },
        ));

        Ok(())
    }
}
