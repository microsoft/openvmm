// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publish the assembled OpenVMM source archive as a draft GitHub release.
//!
//! Publication is deliberately two-stage: this job only ever creates a
//! *draft*. A maintainer reviews the draft and clicks Publish, and GitHub
//! creates the `openvmm-v<VERSION>` tag at the commit this job pinned. The tag
//! therefore never exists for a release nobody approved, and the irreversible
//! step stays a human one.

use crate::assemble_openvmm_source_release::CHECKSUM_FILE;
use crate::assemble_openvmm_source_release::SourceReleaseOutput;
use crate::assemble_openvmm_source_release::read_source_identity;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// The assembled archive and its checksums.
        pub release: ReadVar<SourceReleaseOutput>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::publish_gh_release::Node>();
        ctx.import::<flowey_lib_common::attest_build_provenance::Node>();
        ctx.import::<flowey_lib_common::use_gh_cli::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { release, done } = request;

        let resolved = ctx.emit_rust_stepv("enumerate source release files", |ctx| {
            let release = release.claim(ctx);
            move |rt| {
                let assets = rt.read(release).assets;
                let identity = read_source_identity(&assets)?;

                // Only the two published files, by absolute path. The identity
                // metadata that rode along in the artifact is internal and is
                // deliberately not published.
                let mut files = Vec::new();
                for name in [identity.archive_name(), CHECKSUM_FILE.to_owned()] {
                    let path = assets.join(&name);
                    if !path.exists() {
                        anyhow::bail!("{name} is missing from the assembled source release");
                    }
                    files.push((path.absolute()?, None));
                }

                Ok((identity, files))
            }
        });

        let identity = resolved.map(ctx, |(identity, _)| identity);
        let files = resolved.map(ctx, |(_, files)| files);

        // Attest the exact files that are about to be uploaded, so a consumer
        // can tie the archive back to the workflow run that produced it.
        let attested = ctx.reqv(|done| flowey_lib_common::attest_build_provenance::Request {
            files: files.clone(),
            done,
        });

        let target = identity.map(ctx, |identity| identity.revision);
        let tag = identity.map(ctx, |identity| format!("openvmm-v{}", identity.version));
        let title = identity.map(ctx, |identity| format!("OpenVMM v{}", identity.version));

        // `gh release create --target` uses an existing tag as-is. Refuse a
        // stray tag so the release cannot silently point at a different commit
        // than the archive identity.
        let gh_cli = ctx.reqv(flowey_lib_common::use_gh_cli::Request::Get);
        let tag_is_unused = ctx.emit_rust_step("ensure source release tag is unused", |ctx| {
            let gh_cli = gh_cli.claim(ctx);
            let tag = tag.clone().claim(ctx);
            move |rt| {
                let gh_cli = rt.read(gh_cli);
                let tag = rt.read(tag);
                let output = flowey::shell_cmd!(
                    rt,
                    "{gh_cli} api repos/microsoft/openvmm/git/ref/tags/{tag}"
                )
                .ignore_status()
                .output()
                .context("failed to query the OpenVMM release tag")?;

                if output.status.success() {
                    anyhow::bail!(
                        "tag {tag} already exists; refusing to create a release whose target \
                         may differ from the assembled source revision"
                    );
                }

                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.contains("HTTP 404") {
                    anyhow::bail!(
                        "failed to query OpenVMM release tag {tag}: {}",
                        stderr.trim()
                    );
                }

                Ok(())
            }
        });

        ctx.req(flowey_lib_common::publish_gh_release::Request(
            flowey_lib_common::publish_gh_release::GhReleaseParams {
                repo_owner: "microsoft".into(),
                repo_name: "openvmm".into(),
                target,
                tag,
                title,
                files,
                // The draft body is written by the maintainer reviewing it.
                // Generated notes would compare against the previous tag, and
                // the first release has no previous tag to compare against.
                notes: flowey_lib_common::publish_gh_release::GhReleaseNotes::Empty,
                draft: true,
                // Unlike a release that tracks every push, this pipeline only
                // runs because someone asked for this version. Quietly doing
                // nothing would look like it worked.
                on_existing: flowey_lib_common::publish_gh_release::OnExistingRelease::Fail,
                prerequisites: vec![attested, tag_is_unused],
                done,
            },
        ));

        Ok(())
    }
}
