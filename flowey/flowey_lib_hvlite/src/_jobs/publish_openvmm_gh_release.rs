// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publishes a github release for OpenVMM

use crate::build_openvmm::OpenvmmOutput;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct OpenvmmGhReleaseArtifacts {
    pub openvmm_windows_x64: ReadVar<OpenvmmOutput>,
    pub openvmm_windows_aarch64: ReadVar<OpenvmmOutput>,
    pub openvmm_linux_x64: ReadVar<OpenvmmOutput>,
    pub openhcl_igvm_files_x64: ReadVar<PathBuf>,
    pub openhcl_igvm_files_aarch64: ReadVar<PathBuf>,
}

flowey_request! {
    pub struct Request {
        pub artifacts: OpenvmmGhReleaseArtifacts,
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
        let Request { artifacts, done } = request;

        let repo = ctx.get_gh_context_var().global().repository();
        let branch = ctx.get_gh_context_var().global().ref_name();
        let run_id = ctx.get_gh_context_var().global().run_id();
        let tag = branch.zip(ctx, run_id).map(ctx, |(b, r)| {
            format!("{}.{r}", b.trim_start_matches("release/"))
        });
        let title = tag.map(ctx, |t| format!("OpenVMM {t}"));
        let target = ctx.get_gh_context_var().global().sha();
        let files = artifacts.into_paths(ctx);
        let gh_token = ctx.get_gh_context_var().global().token();

        ctx.req(flowey_lib_common::publish_gh_release::Request {
            repo,
            tag,
            title,
            target,
            files,
            gh_token,
            done,
        });

        Ok(())
    }
}

impl OpenvmmGhReleaseArtifacts {
    fn into_paths(self, ctx: &mut NodeCtx<'_>) -> Vec<ReadVar<PathBuf>> {
        vec![
            self.openvmm_windows_x64.map(ctx, |x| match x {
                OpenvmmOutput::WindowsBin { exe, .. } => exe,
                _ => unreachable!(),
            }),
            self.openvmm_windows_aarch64.map(ctx, |x| match x {
                OpenvmmOutput::WindowsBin { exe, .. } => exe,
                _ => unreachable!(),
            }),
            self.openvmm_linux_x64.map(ctx, |x| match x {
                OpenvmmOutput::LinuxBin { bin, .. } => bin,
                _ => unreachable!(),
            }),
            self.openhcl_igvm_files_x64.map(ctx, |x| x.join("*")),
            self.openhcl_igvm_files_aarch64.map(ctx, |x| x.join("*")),
        ]
    }
}
