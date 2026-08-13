// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publish the assembled OpenVMM source archive as a draft GitHub release.
//!
//! Publication is deliberately two-stage: this job only ever creates the tag
//! and a *draft*. A maintainer reviews the draft and clicks Publish. The
//! irreversible release step therefore stays with a human, while the release
//! cannot be rebound to another commit during review.

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

        // Refuse an existing release before creating the tag. The tag is a
        // side effect this job cannot take back, and a release that already
        // exists for this version means a rerun would otherwise pin a tag that
        // the pre-existing release silently adopts.
        let gh_cli = ctx.reqv(flowey_lib_common::use_gh_cli::Request::Get);
        let no_existing_release = ctx.emit_rust_step("ensure no existing source release", |ctx| {
            let gh_cli = gh_cli.clone().claim(ctx);
            let tag = tag.clone().claim(ctx);
            move |rt| {
                let gh_cli = rt.read(gh_cli);
                let tag = rt.read(tag);

                let output =
                    flowey::shell_cmd!(rt, "{gh_cli} release view {tag} --repo microsoft/openvmm")
                        .ignore_status()
                        .output()
                        .context("failed to query the OpenVMM release")?;

                if output.status.success() {
                    anyhow::bail!(
                        "a GitHub release already exists for tag {tag}. It may already have \
                         been reviewed or published, so this run will not pin a tag it could \
                         adopt. Delete it and rerun if it should be regenerated."
                    );
                }

                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.contains("release not found") {
                    anyhow::bail!(
                        "failed to query the OpenVMM release for tag {tag}: {}",
                        stderr.trim()
                    );
                }

                Ok(())
            }
        });

        // Create the tag before the draft so a later tag cannot silently rebind
        // the release to a different commit. Reruns reuse it only when it still
        // names the exact archived revision.
        let tag_is_pinned = ctx.emit_rust_step("pin source release tag", |ctx| {
            // Claiming without reading is what orders this step after the
            // check. The side effect a rust step hands back is never written to
            // the var db, so reading it at runtime would panic.
            no_existing_release.claim(ctx);
            let gh_cli = gh_cli.claim(ctx);
            let tag = tag.clone().claim(ctx);
            let target = target.clone().claim(ctx);
            move |rt| {
                let gh_cli = rt.read(gh_cli);
                let tag = rt.read(tag);
                let target = rt.read(target);

                let ref_name = format!("refs/tags/{tag}");
                let create = flowey::shell_cmd!(
                    rt,
                    "{gh_cli} api --method POST repos/microsoft/openvmm/git/refs -f ref={ref_name} -f sha={target}"
                )
                .ignore_status()
                .output()
                .context("failed to create the OpenVMM release tag")?;
                if create.status.success() {
                    log::info!("created release tag {tag} at {target}");
                    return Ok(());
                }

                let existing = flowey::shell_cmd!(
                    rt,
                    "{gh_cli} api repos/microsoft/openvmm/git/ref/tags/{tag}"
                )
                .ignore_status()
                .output()
                .context("failed to query the existing OpenVMM release tag")?;
                if !existing.status.success() {
                    anyhow::bail!(
                        "failed to create release tag {tag}: {}; querying the existing tag also \
                         failed: {}",
                        String::from_utf8_lossy(&create.stderr).trim(),
                        String::from_utf8_lossy(&existing.stderr).trim()
                    );
                }

                let existing: serde_json::Value = serde_json::from_slice(&existing.stdout)
                    .context("failed to parse the existing OpenVMM release tag")?;
                let tag_type = existing["object"]["type"].as_str();
                let tag_target = existing["object"]["sha"].as_str();
                if tag_type == Some("tag") {
                    // `object.sha` names the annotation, not a commit, so it
                    // cannot be compared against the archived revision.
                    anyhow::bail!(
                        "release tag {tag} already exists as an annotated tag (object {}), but \
                         this pipeline publishes lightweight tags naming commit {target}",
                        tag_target.unwrap_or("<unknown>")
                    );
                }
                if tag_type != Some("commit") || tag_target != Some(target.as_str()) {
                    anyhow::bail!(
                        "release tag {tag} already exists at {} ({}) instead of commit {target}",
                        tag_target.unwrap_or("<unknown>"),
                        tag_type.unwrap_or("unknown object type")
                    );
                }

                log::info!("reusing release tag {tag} at {target}");
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
                verify_tag: true,
                // Unlike a release that tracks every push, this pipeline only
                // runs because someone asked for this version. Quietly doing
                // nothing would look like it worked.
                on_existing: flowey_lib_common::publish_gh_release::OnExistingRelease::Fail,
                prerequisites: vec![attested, tag_is_pinned],
                done,
            },
        ));

        Ok(())
    }
}
