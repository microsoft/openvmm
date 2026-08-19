// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Create a tag and draft GitHub release for OpenVMM.
//!
//! Publication is deliberately two-stage: this job only ever creates the tag
//! and a *draft*. A maintainer reviews the draft and clicks Publish. The
//! irreversible release step therefore stays with a human, while the release
//! cannot be rebound to another commit during review.
//!
//! GitHub automatically provides source archives for the release tag. This job
//! uploads the vendor archive required for offline Cargo builds and raw Linux
//! binaries. Unsigned Windows outputs remain workflow artifacts.

use crate::assemble_openvmm_vendor_release::{VendorReleaseOutput, read_vendor_identity};
use crate::build_openvmm::OpenvmmOutput;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub release: ReadVar<VendorReleaseOutput>,
        pub linux_x64: ReadVar<OpenvmmOutput>,
        pub linux_aarch64: ReadVar<OpenvmmOutput>,
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
        let Request {
            release,
            linux_x64,
            linux_aarch64,
            done,
        } = request;

        let files = ctx.emit_rust_stepv("enumerate release files", |ctx| {
            let release = release.clone().claim(ctx);
            let linux_x64 = linux_x64.claim(ctx);
            let linux_aarch64 = linux_aarch64.claim(ctx);
            move |rt| {
                let release = rt.read(release);
                let linux_x64 = rt.read(linux_x64);
                let linux_aarch64 = rt.read(linux_aarch64);
                let stage_dir = std::env::current_dir()?;
                openvmm_release_files(&release.assets, linux_x64, linux_aarch64, &stage_dir)
            }
        });

        let identity = ctx.emit_rust_stepv("resolve release identity", |ctx| {
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
        let no_existing_release = ctx.emit_rust_step("ensure no existing release", |ctx| {
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
        let tag_is_pinned = ctx.emit_rust_step("pin release tag", |ctx| {
            // Claiming without reading is what orders this step after the
            // check. The side effect a rust step hands back is never written to
            // the var db, so reading it at runtime would panic.
            no_existing_release.claim(ctx);
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

fn openvmm_release_files(
    assets: &Path,
    linux_x64: OpenvmmOutput,
    linux_aarch64: OpenvmmOutput,
    stage_dir: &Path,
) -> anyhow::Result<Vec<(PathBuf, Option<String>)>> {
    let identity = read_vendor_identity(assets)?;
    let archive = assets.join(identity.archive_name());
    if !archive.is_file() {
        anyhow::bail!("missing vendor archive {}", archive.display());
    }

    let mut files = vec![(archive, None)];
    files.push(stage_linux_binary(
        linux_x64,
        "x86_64-unknown-linux-musl",
        stage_dir,
    )?);
    files.push(stage_linux_binary(
        linux_aarch64,
        "aarch64-unknown-linux-musl",
        stage_dir,
    )?);
    Ok(files)
}

fn stage_linux_binary(
    output: OpenvmmOutput,
    target: &str,
    stage_dir: &Path,
) -> anyhow::Result<(PathBuf, Option<String>)> {
    let OpenvmmOutput::LinuxBin { bin, dbg: _ } = output else {
        anyhow::bail!("expected a Linux OpenVMM binary for {target}");
    };
    if !bin.is_file() {
        anyhow::bail!("missing OpenVMM binary {}", bin.display());
    }

    let staged = stage_dir.join(format!("openvmm-{target}"));
    fs_err::hard_link(&bin, &staged)
        .with_context(|| format!("failed to stage {} as {}", bin.display(), staged.display()))?;
    Ok((staged.absolute()?, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble_openvmm_vendor_release::{IDENTITY_FILE, VendorReleaseIdentity};

    #[test]
    fn release_assets_include_vendor_and_raw_linux_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let stage_dir = tempfile::tempdir().unwrap();
        let identity = VendorReleaseIdentity {
            version: "0.12.3".into(),
            revision: "0123456789abcdef0123456789abcdef01234567".into(),
        };
        let archive = dir.path().join(identity.archive_name());
        let x64 = dir.path().join("x64-openvmm");
        let aarch64 = dir.path().join("aarch64-openvmm");

        fs_err::write(
            dir.path().join(IDENTITY_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        fs_err::write(&archive, b"archive").unwrap();
        fs_err::write(&x64, b"x64").unwrap();
        fs_err::write(&aarch64, b"aarch64").unwrap();

        assert_eq!(
            openvmm_release_files(
                dir.path(),
                OpenvmmOutput::LinuxBin {
                    bin: x64,
                    dbg: dir.path().join("x64.dbg"),
                },
                OpenvmmOutput::LinuxBin {
                    bin: aarch64,
                    dbg: dir.path().join("aarch64.dbg"),
                },
                stage_dir.path(),
            )
            .unwrap(),
            vec![
                (archive, None),
                (
                    stage_dir
                        .path()
                        .join("openvmm-x86_64-unknown-linux-musl")
                        .absolute()
                        .unwrap(),
                    None,
                ),
                (
                    stage_dir
                        .path()
                        .join("openvmm-aarch64-unknown-linux-musl")
                        .absolute()
                        .unwrap(),
                    None,
                ),
            ]
        );
        assert_eq!(
            fs_err::read(stage_dir.path().join("openvmm-x86_64-unknown-linux-musl")).unwrap(),
            b"x64"
        );
        assert_eq!(
            fs_err::read(stage_dir.path().join("openvmm-aarch64-unknown-linux-musl")).unwrap(),
            b"aarch64"
        );
    }

    #[test]
    fn rejects_windows_output_as_a_linux_release_asset() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            stage_linux_binary(
                OpenvmmOutput::WindowsBin {
                    exe: dir.path().join("openvmm.exe"),
                    pdb: None,
                },
                "x86_64-unknown-linux-musl",
                dir.path(),
            )
            .is_err()
        );
    }
}
