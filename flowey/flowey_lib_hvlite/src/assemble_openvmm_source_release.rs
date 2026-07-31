// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assemble the OpenVMM source release.
//!
//! This produces the files a release publishes: a `.tar.gz` of the tracked
//! source at a single revision, and a `SHA256SUMS` covering it.
//!
//! Nothing is stamped into the archive. The version is `[workspace.package]
//! version` in the repository's own `Cargo.toml`, so it is already inside the
//! tree `git archive` exports, and a packager building without a `.git`
//! directory recovers it the same way `cargo` does. That is what lets this be a
//! plain `git archive` with no injected metadata: there is no second copy of
//! the version that could disagree with the first.
//!
//! The node is deliberately shared between the release, which publishes these
//! files, and CI, which builds them. CI would otherwise be testing a lookalike
//! rather than the thing that actually ships.
//!
//! Assembly is reproducible: `git archive` emits a deterministic tar for a
//! given commit, and `gzip -n` omits the timestamp that would otherwise vary.
//! Two jobs at the same commit therefore produce the same bytes, which is what
//! lets the job that builds a release and the job that publishes it each
//! assemble independently rather than passing an artifact between them.

use flowey::node::prelude::*;

/// Prefix of the Git tag naming a release.
pub const RELEASE_TAG_PREFIX: &str = "openvmm-v";

/// Checksums covering every published asset.
pub const CHECKSUM_FILE: &str = "SHA256SUMS";

/// The identity of a source release.
///
/// Both fields are read out of the tree rather than out of the environment, so
/// two jobs at the same commit necessarily agree and cannot drift apart later
/// by one of them growing a rule the other does not have.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SourceIdentity {
    /// The workspace version, e.g. `0.12.3`.
    pub version: String,
    /// The full commit the archive was produced from.
    pub revision: String,
}

impl SourceIdentity {
    /// The name of the directory at the root of the archive.
    pub fn source_root(&self) -> String {
        format!("openvmm-{}", self.version)
    }

    /// The name of the source archive.
    pub fn archive_name(&self) -> String {
        format!("{}-source.tar.gz", self.source_root())
    }

    /// The Git tag naming this release.
    ///
    /// Derived, not parsed. The version in the tree is the single source of
    /// truth, and the tag is one of its consequences.
    pub fn release_tag(&self) -> String {
        format!("{RELEASE_TAG_PREFIX}{}", self.version)
    }
}

/// Resolve the identity of the OpenVMM checkout in the current working
/// directory.
pub fn resolve_identity(rt: &mut RustRuntimeServices<'_>) -> anyhow::Result<SourceIdentity> {
    let revision = flowey::shell_cmd!(rt, "git rev-parse HEAD").read()?;
    let version = workspace_version(&std::env::current_dir()?.join("Cargo.toml"))?;

    Ok(SourceIdentity { version, revision })
}

/// Read `[workspace.package] version` out of a workspace manifest.
fn workspace_version(manifest_path: &Path) -> anyhow::Result<String> {
    let manifest = fs_err::read_to_string(manifest_path)?
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;

    let version = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .and_then(|package| package.get("version"))
        .with_context(|| {
            format!(
                "{} has no [workspace.package] version",
                manifest_path.display()
            )
        })?
        .as_str()
        .context("[workspace.package] version is not a string")?;

    // The version reaches the archive prefix, the asset names, and the release
    // title, so a value that is not a plain version would be visible in all of
    // them. Everything cargo accepts here is fine; everything else is not.
    if version.is_empty() || version.contains(['/', '\\', ' ']) {
        anyhow::bail!("[workspace.package] version is not usable as a name: {version:?}");
    }

    Ok(version.to_owned())
}

flowey_request! {
    pub struct Request {
        /// Identity to assemble under.
        pub identity: ReadVar<SourceIdentity>,
        /// Directory to assemble the release assets into. Created if absent.
        pub output_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            identity,
            output_dir,
            done,
        } = request;

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        ctx.emit_rust_step("assemble OpenVMM source release", |ctx| {
            done.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            let identity = identity.claim(ctx);
            let output_dir = output_dir.claim(ctx);
            move |rt| {
                let identity = rt.read(identity);
                let output_dir = rt.read(output_dir);
                let repo_path = rt.read(openvmm_repo_path);

                fs_err::create_dir_all(&output_dir)?;
                rt.sh.change_dir(&repo_path);

                // `git archive` exports the tree at HEAD, so an uncommitted
                // change would silently not appear in the archive. That would
                // publish something other than what was built and tested.
                let dirty =
                    flowey::shell_cmd!(rt, "git status --porcelain --untracked-files=no").read()?;
                if !dirty.trim().is_empty() {
                    anyhow::bail!(
                        "refusing to assemble a source release from a dirty working tree; \
                         the archive would not match HEAD.\nmodified:\n{dirty}"
                    );
                }

                // `tar.umask` masks the mode of every entry, so pin it rather
                // than inheriting whatever the machine has configured. This is
                // the same class of hazard as `tar.tgz.command` below.
                let prefix = format!("{}/", identity.source_root());
                let source_tar = output_dir.join("openvmm-source.tar");
                flowey::shell_cmd!(
                    rt,
                    "git -c tar.umask=0002 archive --format=tar --output {source_tar} --prefix={prefix} HEAD"
                )
                .run()?;

                // `-n` omits the timestamp and original name from the gzip
                // header, which is what makes the result reproducible. Do not
                // use `git archive --format=tar.gz`: it defers to the
                // `tar.tgz.command` config, so reproducibility would depend on
                // the machine's git configuration.
                let source_archive = output_dir.join(identity.archive_name());
                let compressed =
                    flowey::shell_cmd!(rt, "gzip -n --best --stdout {source_tar}").output()?;
                fs_err::write(&source_archive, compressed.stdout)?;
                fs_err::remove_file(source_tar)?;

                // Checksums are a published asset, so generate them here rather
                // than in the publishing job. That way CI verifies the same
                // file a consumer will check against.
                let archive_name = identity.archive_name();
                rt.sh.change_dir(&output_dir);
                let checksums = flowey::shell_cmd!(rt, "sha256sum {archive_name}").output()?;
                fs_err::write(output_dir.join(CHECKSUM_FILE), checksums.stdout)?;

                Ok(())
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(version: &str) -> SourceIdentity {
        SourceIdentity {
            version: version.into(),
            revision: "0123456789abcdef0123456789abcdef01234567".into(),
        }
    }

    #[test]
    fn asset_names_follow_the_version() {
        let identity = identity("0.12.3");
        assert_eq!(identity.source_root(), "openvmm-0.12.3");
        assert_eq!(identity.archive_name(), "openvmm-0.12.3-source.tar.gz");
        assert_eq!(identity.release_tag(), "openvmm-v0.12.3");
    }

    #[test]
    fn reads_the_workspace_version() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");

        fs_err::write(
            &manifest,
            "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.12.3-dev\"\n",
        )
        .unwrap();
        assert_eq!(workspace_version(&manifest).unwrap(), "0.12.3-dev");

        // A manifest that does not declare one at all, rather than one that
        // declares something unusable, is the likely failure if the version
        // ever moves.
        fs_err::write(&manifest, "[workspace]\nmembers = []\n").unwrap();
        assert!(workspace_version(&manifest).is_err());

        // `version.workspace = true` is a table, not a string. Pointing this at
        // a member crate's manifest must fail loudly rather than produce a
        // nonsense name.
        fs_err::write(
            &manifest,
            "[workspace.package]\nversion = { workspace = true }\n",
        )
        .unwrap();
        assert!(workspace_version(&manifest).is_err());

        // Anything that would escape the archive prefix or an asset name.
        fs_err::write(&manifest, "[workspace.package]\nversion = \"0.1.0/x\"\n").unwrap();
        assert!(workspace_version(&manifest).is_err());
    }
}
