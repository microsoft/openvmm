// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generate GitHub build provenance attestations for a set of files.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub files: ReadVar<Vec<(PathBuf, Option<String>)>>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { files, done } = request;
        let subject_paths = files.map(ctx, |files| {
            files
                .into_iter()
                .map(|(path, _)| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("\n")
        });

        let attested = if matches!(ctx.backend(), FlowBackend::Github) {
            ctx.emit_gh_step("Attest release artifacts", "actions/attest@v4")
                .with("subject-path", subject_paths)
                .requires_permission(GhPermission::Contents, GhPermissionValue::Read)
                .requires_permission(GhPermission::IdToken, GhPermissionValue::Write)
                .requires_permission(GhPermission::Attestations, GhPermissionValue::Write)
                .requires_permission(GhPermission::ArtifactMetadata, GhPermissionValue::Write)
                .finish(ctx)
        } else {
            ctx.emit_rust_step("(stub) attest release artifacts", |ctx| {
                subject_paths.claim(ctx);
                |_rt| {
                    log::warn!("not running in GitHub Actions, so no attestation was generated");
                    Ok(())
                }
            })
        };

        ctx.emit_side_effect_step([attested], [done]);
        Ok(())
    }
}
