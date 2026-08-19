// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Create a tag and draft GitHub release for OpenVMM.
//!
//! Publication is deliberately two-stage: this job only ever creates the tag
//! and a *draft*. A maintainer reviews the draft and clicks Publish. The
//! irreversible release step therefore stays with a human, while the release
//! cannot be rebound to another commit during review.
//!
//! GitHub automatically provides source archives for the release tag. The only
//! uploaded asset is the vendor archive required for offline Cargo builds.
//!
//! A dispatch may name any ref, so this job confirms that the archived revision
//! is contained in a protected `main` or `release/*` branch before tagging it.

use crate::assemble_openvmm_vendor_release::{VendorReleaseOutput, read_vendor_identity};
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub release: ReadVar<VendorReleaseOutput>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::publish_gh_release::Node>();
        ctx.import::<flowey_lib_common::use_gh_cli::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { release, done } = request;

        let files = ctx.emit_rust_stepv("enumerate release files", |ctx| {
            let release = release.clone().claim(ctx);
            move |rt| {
                let release = rt.read(release);
                vendor_release_files(&release.assets)
            }
        });

        let identity = ctx.emit_rust_stepv("resolve source release identity", |ctx| {
            let release = release.claim(ctx);
            move |rt| {
                let release = rt.read(release);
                read_vendor_identity(&release.assets)
            }
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

        let commit_is_reviewed = ctx.emit_rust_step("verify release commit is reviewed", |ctx| {
            let gh_cli = gh_cli.clone().claim(ctx);
            let target = target.clone().claim(ctx);
            move |rt| {
                let gh_cli = rt.read(gh_cli);
                let revision = rt.read(target);

                let branches = flowey::shell_cmd!(
                    rt,
                    "{gh_cli} api repos/microsoft/openvmm/branches?protected=true&per_page=100"
                )
                .read()
                .context("failed to list the protected branches")?;
                let candidates = candidate_branches(
                    &serde_json::from_str(&branches)
                        .context("failed to parse the protected branch list")?,
                )?;

                for branch in &candidates {
                    let compare = flowey::shell_cmd!(
                        rt,
                        "{gh_cli} api repos/microsoft/openvmm/compare/{revision}...{branch}?per_page=1"
                    )
                    .read()
                    .with_context(|| format!("failed to compare {revision} against {branch}"))?;
                    let compare = serde_json::from_str(&compare)
                        .with_context(|| format!("failed to parse the comparison against {branch}"))?;
                    if !branch_contains(&revision, &compare)? {
                        continue;
                    }

                    log::info!("{revision} is contained in reviewed branch {branch}");
                    return Ok(());
                }

                anyhow::bail!(
                    "commit {revision} is not contained in any protected `main` or `release/*` \
                     branch (checked: {}). A release is cut from reviewed history, so the \
                     commit must be merged first.",
                    candidates.join(", ")
                )
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
            commit_is_reviewed.claim(ctx);
            // Order the archive-existence check ahead of the tag as well. The
            // tag cannot be taken back, so an artifact that arrived without its
            // vendor archive must fail before the tag exists, not after.
            let _files = files.clone().claim(ctx);
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
                prerequisites: vec![tag_is_pinned],
                done,
            },
        ));

        Ok(())
    }
}

/// The protected branches a release may be cut from, `main` first.
fn candidate_branches(branches: &serde_json::Value) -> anyhow::Result<Vec<String>> {
    let branches = branches
        .as_array()
        .context("expected the protected branch list to be an array")?;

    let mut names = Vec::new();
    for branch in branches {
        let name = branch["name"]
            .as_str()
            .context("expected every protected branch to be named")?;
        names.push(name.to_owned());
    }

    // Fail closed if `protected` ever stops excluding unreviewed namespaces.
    names.retain(|name| name == "main" || name.starts_with("release/"));

    // Stable sorts, so `main` is tried before the release branches.
    names.sort();
    names.sort_by_key(|name| name != "main");
    Ok(names)
}

/// Whether `revision` is contained in the branch `compare` was taken against.
///
/// Containment, not tip equality: the branch advances while the release builds.
fn branch_contains(revision: &str, compare: &serde_json::Value) -> anyhow::Result<bool> {
    let merge_base = compare["merge_base_commit"]["sha"]
        .as_str()
        .context("expected the comparison to name a merge base")?;
    Ok(merge_base == revision)
}

fn vendor_release_files(assets: &Path) -> anyhow::Result<Vec<(PathBuf, Option<String>)>> {
    let identity = read_vendor_identity(assets)?;
    let archive = assets.join(identity.archive_name());
    if !archive.is_file() {
        anyhow::bail!("missing vendor archive {}", archive.display());
    }

    Ok(vec![(archive, None)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble_openvmm_vendor_release::{IDENTITY_FILE, VendorReleaseIdentity};

    #[test]
    fn release_candidates_are_protected_branches_with_main_first() {
        let branches = serde_json::json!([
            { "name": "release/2505", "protected": true },
            { "name": "release/1.8.2607", "protected": true },
            { "name": "main", "protected": true },
            { "name": "release/1.7.2511", "protected": true },
        ]);

        assert_eq!(
            candidate_branches(&branches).unwrap(),
            [
                "main",
                "release/1.7.2511",
                "release/1.8.2607",
                "release/2505"
            ]
        );
    }

    #[test]
    fn a_branch_that_is_not_a_release_branch_is_not_a_candidate() {
        let branches = serde_json::json!([
            { "name": "main", "protected": true },
            { "name": "copilot/some-branch", "protected": true },
        ]);

        assert_eq!(candidate_branches(&branches).unwrap(), ["main"]);
    }

    #[test]
    fn a_merged_revision_is_contained_in_the_branch() {
        let revision = "ee2fc3f0000000000000000000000000000000ee";
        let compare = serde_json::json!({
            "status": "ahead",
            "merge_base_commit": { "sha": revision },
        });

        assert!(branch_contains(revision, &compare).unwrap());
    }

    #[test]
    fn an_unmerged_revision_is_not_contained_in_the_branch() {
        let compare = serde_json::json!({
            "status": "diverged",
            "merge_base_commit": { "sha": "6bb401ccbb840000000000000000000000000000" },
        });

        assert!(!branch_contains("c8c65e554b64bb2042d94fc612719d6c1e767235", &compare).unwrap());
    }

    #[test]
    fn identity_stays_private_when_enumerating_release_assets() {
        let dir = tempfile::tempdir().unwrap();
        let identity = VendorReleaseIdentity {
            version: "0.12.3".into(),
            revision: "0123456789abcdef0123456789abcdef01234567".into(),
        };
        let archive = dir.path().join(identity.archive_name());

        fs_err::write(
            dir.path().join(IDENTITY_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        fs_err::write(&archive, b"archive").unwrap();

        assert_eq!(
            vendor_release_files(dir.path()).unwrap(),
            vec![(archive, None)]
        );
    }
}
