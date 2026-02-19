// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publishes a GitHub release for VmgsTool

use crate::build_vmgstool::VmgstoolOutput;
use crate::build_vmgstool::VmgstoolOutputBin;
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
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { vmgstools, done } = request;

        let files_and_version = ctx.emit_rust_stepv("enumerate vmgstool release files", |ctx| {
            let vmgstools = vmgstools
                .into_iter()
                .map(|(t, v)| (t, v.claim(ctx)))
                .collect::<BTreeMap<_, _>>();
            move |rt| {
                let mut files = Vec::new();
                let mut version = None;
                for (target, vmgstool) in vmgstools {
                    let vmgstool = rt.read(vmgstool);
                    match vmgstool.bin {
                        VmgstoolOutputBin::LinuxBin { bin, dbg } => {
                            files.push((bin, Some(format!("vmgstool-{target}"))));
                            files.push((dbg, Some(format!("vmgstool-{target}.dbg"))));
                        }
                        VmgstoolOutputBin::WindowsBin { exe, pdb } => {
                            files.push((exe, Some(format!("vmgstool-{target}.exe"))));
                            files.push((pdb, Some(format!("vmgstool-{target}.pdb"))));
                        }
                    }
                    if let Some(version) = &version {
                        assert_eq!(version, &vmgstool.version);
                    } else {
                        version = Some(vmgstool.version);
                    }
                }
                Ok((files, version.expect("no vmgstools")))
            }
        });

        let tag = files_and_version.map(ctx, |(_, v)| format!("vmgstool-v{v}"));
        let title = files_and_version.map(ctx, |(_, v)| format!("VmgsTool v{v}"));
        let files = files_and_version.map(ctx, |(f, _)| f);

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
