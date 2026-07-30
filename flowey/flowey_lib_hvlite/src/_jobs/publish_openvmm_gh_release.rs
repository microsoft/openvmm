// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assemble, attest, and publish a standalone OpenVMM source release.
//!
//! OpenVMM releases source first. The published assets are the source archive
//! and its checksums; prebuilt binaries are a later phase, and deliberately not
//! part of this pipeline, so releasing does not depend on binary signing.
//!
//! The assets are assembled with the same node CI builds from, and assembly is
//! reproducible, so what is published here is what the distribution build job
//! already proved buildable.

use crate::assemble_openvmm_source_release::CHECKSUM_FILE;
use crate::assemble_openvmm_source_release::IdentitySource;
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
        ctx.import::<flowey_lib_common::attest_build_provenance::Node>();
        ctx.import::<flowey_lib_common::publish_gh_release::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { done } = request;

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let resolved = ctx.emit_rust_stepv("resolve OpenVMM release identity", |ctx| {
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            move |rt| {
                let output_dir = std::env::current_dir()?.join("openvmm-source-release");
                let path = rt.read(openvmm_repo_path);
                rt.sh.change_dir(path);

                let identity = IdentitySource::ReleaseTag.resolve(rt)?;

                Ok((identity, output_dir))
            }
        });
        let identity = resolved.clone().map(ctx, |(identity, _)| identity);
        let output_dir = resolved.map(ctx, |(_, output_dir)| output_dir);

        let assembled = ctx.reqv(|done| crate::assemble_openvmm_source_release::Request {
            identity: identity.clone(),
            output_dir: output_dir.clone(),
            done,
        });

        let files = output_dir
            .depending_on(ctx, &assembled)
            .zip(ctx, identity.clone())
            .map(ctx, |(output_dir, identity)| {
                // Name the assets explicitly rather than globbing the
                // directory, so nothing incidental can end up on the release.
                vec![
                    (output_dir.join(identity.archive_name()), None),
                    (output_dir.join(CHECKSUM_FILE), None),
                ]
            });

        let target = identity.clone().map(ctx, |identity| identity.revision);
        let tag = identity.clone().map(ctx, |identity| {
            identity
                .tag
                .expect("a release is always assembled from a tag")
        });
        let title = identity.map(ctx, |identity| format!("OpenVMM v{}", identity.version));

        let (attestation_done, write_attestation_done) = ctx.new_var();
        ctx.req(flowey_lib_common::attest_build_provenance::Request {
            files: files.clone(),
            done: write_attestation_done,
        });
        ctx.req(flowey_lib_common::publish_gh_release::Request(
            flowey_lib_common::publish_gh_release::GhReleaseParams {
                repo_owner: "microsoft".into(),
                repo_name: "openvmm".into(),
                target,
                tag,
                title,
                files,
                notes: flowey_lib_common::publish_gh_release::GhReleaseNotes::Generated,
                // Publish as a draft. Releasing is new enough that a human
                // should look at the assembled release before it is public.
                draft: true,
                // This runs once, when a release tag is pushed. A release that
                // already exists means an earlier attempt got this far, so say
                // so rather than reporting success without doing anything.
                // Regenerating a draft means deleting it first, which is
                // deliberate: assets are never replaced automatically.
                on_existing: flowey_lib_common::publish_gh_release::OnExistingRelease::Fail,
                prerequisites: vec![attestation_done],
                done,
            },
        ));

        Ok(())
    }
}
