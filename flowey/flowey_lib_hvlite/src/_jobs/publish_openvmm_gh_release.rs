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
use flowey::node::prelude::*;

fn stable_version(version: &str) -> anyhow::Result<(u64, u64, u64)> {
    let components = version
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>();
    let Ok(components) = components else {
        anyhow::bail!("{version:?} is not a stable MAJOR.MINOR.PATCH version");
    };
    let [major, minor, patch] = components.as_slice() else {
        anyhow::bail!("{version:?} is not a stable MAJOR.MINOR.PATCH version");
    };
    if version != format!("{major}.{minor}.{patch}") {
        anyhow::bail!("{version:?} is not a canonical MAJOR.MINOR.PATCH version");
    }
    Ok((*major, *minor, *patch))
}

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
                rt.sh.change_dir(&path);

                let identity = crate::assemble_openvmm_source_release::resolve_identity(rt)?;

                let version = stable_version(&identity.version)?;

                // The ref selected in the workflow UI may be a branch or tag,
                // but releases must come from committed mainline history.
                flowey::shell_cmd!(
                    rt,
                    "git fetch --no-tags --unshallow origin +refs/heads/main:refs/remotes/origin/main"
                )
                .run()?;
                let reachable = std::process::Command::new("git")
                    .args(["merge-base", "--is-ancestor", "HEAD", "origin/main"])
                    .current_dir(&path)
                    .status()
                    .context("failed to check whether the release commit is on main")?;
                if !reachable.success() {
                    anyhow::bail!(
                        "{} is not reachable from origin/main; OpenVMM releases must come from \
                         mainline history",
                        identity.revision
                    );
                }

                // The workspace version advances in a reviewed pull request.
                // Require it to be newer than every stable OpenVMM release tag,
                // so dispatching an old branch cannot recreate an old release.
                let existing =
                    flowey::shell_cmd!(rt, "git ls-remote --tags origin refs/tags/openvmm-v*")
                        .read()?;
                let latest = existing
                    .lines()
                    .filter_map(|line| line.split_whitespace().nth(1))
                    .filter_map(|name| name.strip_prefix("refs/tags/openvmm-v"))
                    .filter_map(|name| name.strip_suffix("^{}").or(Some(name)))
                    .filter_map(|name| stable_version(name).ok())
                    .max();
                if let Some(latest) = latest
                    && version <= latest
                {
                    anyhow::bail!(
                        "OpenVMM {} is not newer than the latest released version {}.{}.{}",
                        identity.version,
                        latest.0,
                        latest.1,
                        latest.2
                    );
                }

                // A published release creates this tag. Refuse it independently
                // of the GitHub release lookup below: tags are what source
                // consumers build, and their meaning is immutable.
                let tag = identity.release_tag();
                let existing_tag =
                    flowey::shell_cmd!(rt, "git ls-remote --tags origin refs/tags/{tag}").read()?;
                if !existing_tag.trim().is_empty() {
                    anyhow::bail!("{tag} already exists; releasing again would redefine it");
                }

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
        let tag = identity.clone().map(ctx, |identity| identity.release_tag());
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
                // should look at the assembled release before it is public --
                // and GitHub does not create a draft release's tag until it is
                // published, so the irreversible step is genuinely last.
                draft: true,
                // A failed run is safely rerunnable: replace an existing draft
                // from this version, but never alter a published release.
                on_existing: flowey_lib_common::publish_gh_release::OnExistingRelease::ReplaceDraft,
                prerequisites: vec![attestation_done],
                done,
            },
        ));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::stable_version;

    #[test]
    fn accepts_only_stable_three_part_versions() {
        assert_eq!(stable_version("1.2.3").unwrap(), (1, 2, 3));
        for invalid in ["1.2", "1.2.3.4", "1.2.3-dev", "v1.2.3", "1.02.3"] {
            assert!(stable_version(invalid).is_err(), "{invalid}");
        }
    }
}
