// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assemble the OpenVMM source archive.
//!
//! This produces the files consumed by the distribution-build gate: a
//! deterministic `.tar.gz` of the tracked source at a single revision, and a
//! `SHA256SUMS` file covering it.
//!
//! Nothing is stamped into the archive. The version is `[workspace.package]
//! version` in the repository's own `Cargo.toml`, so it is already inside the
//! tree `git archive` exports, and a packager building without a `.git`
//! directory recovers it the same way `cargo` does. That is what lets this be a
//! plain `git archive` with no injected metadata: there is no second copy of
//! the version that could disagree with the first.
//!
//! The node is shared by CI and the release pipeline so that CI builds the
//! exact source artifact intended for distribution rather than a lookalike.
//!
//! Assembly is reproducible: `git archive` emits a deterministic tar for a
//! given commit, and `gzip -n` omits the timestamp that would otherwise vary.

use flowey::node::prelude::*;

/// Checksums covering the assembled source archive.
pub const CHECKSUM_FILE: &str = "SHA256SUMS";

/// Internal identity stored alongside the assembled assets.
///
/// Flowey includes hidden files when transferring typed artifact directories.
const IDENTITY_FILE: &str = ".openvmm-source-identity.json";

/// The identity of an OpenVMM source archive.
///
/// Both fields are read out of the tree rather than out of the environment, so
/// two jobs at the same commit necessarily agree and cannot drift apart.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SourceIdentity {
    /// The workspace version, e.g. `0.12.3`.
    pub version: String,
    /// The full commit the archive was produced from.
    pub revision: String,
}

/// The assembled source archive transferred between jobs.
#[derive(Serialize, Deserialize)]
pub struct SourceReleaseOutput {
    /// Directory containing the source archive, [`CHECKSUM_FILE`], and internal
    /// identity metadata.
    pub assets: PathBuf,
}

impl Artifact for SourceReleaseOutput {}

/// Read the identity transferred with assembled source assets.
pub fn read_source_identity(assets: &Path) -> anyhow::Result<SourceIdentity> {
    let path = assets.join(IDENTITY_FILE);
    let contents =
        fs_err::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&contents).with_context(|| format!("failed to parse {}", path.display()))
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
}

/// Resolve the identity of the OpenVMM checkout in the current working
/// directory.
pub fn resolve_identity(rt: &mut RustRuntimeServices<'_>) -> anyhow::Result<SourceIdentity> {
    let revision = flowey::shell_cmd!(rt, "git rev-parse HEAD")
        .read()?
        .trim()
        .to_owned();
    let manifest_path = rt.sh.current_dir().absolute()?.join("Cargo.toml");
    let version = workspace_version(&manifest_path)?;

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

    if version.is_empty() || version.contains(['/', '\\', ' ']) {
        anyhow::bail!("[workspace.package] version is not usable as a name: {version:?}");
    }

    Ok(version.to_owned())
}

flowey_request! {
    pub struct Request {
        /// Identity to assemble under.
        pub identity: ReadVar<SourceIdentity>,
        /// Directory to assemble the source assets into. Created if absent.
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

        ctx.emit_rust_step("assemble OpenVMM source archive", |ctx| {
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

                let head = flowey::shell_cmd!(rt, "git rev-parse HEAD").read()?;
                if identity.revision.trim() != head.trim() {
                    anyhow::bail!(
                        "source archive identity revision {} does not match HEAD {}",
                        identity.revision,
                        head.trim()
                    );
                }

                let version = workspace_version(&repo_path.join("Cargo.toml"))?;
                if identity.version != version {
                    anyhow::bail!(
                        "source archive identity version {} does not match workspace version {}",
                        identity.version,
                        version
                    );
                }

                // `git archive` exports the tree at HEAD, so an uncommitted
                // change would silently not appear in the archive.
                let dirty =
                    flowey::shell_cmd!(rt, "git status --porcelain --untracked-files=no").read()?;
                if !dirty.trim().is_empty() {
                    anyhow::bail!(
                        "refusing to assemble a source archive from a dirty working tree; \
                         the archive would not match HEAD.\nmodified:\n{dirty}"
                    );
                }

                // Pin the mode mask rather than inheriting machine-specific
                // Git configuration.
                let prefix = format!("{}/", identity.source_root());
                let source_tar = output_dir.join("openvmm-source.tar");
                flowey::shell_cmd!(
                    rt,
                    "git -c tar.umask=0002 archive --format=tar --output {source_tar} --prefix={prefix} HEAD"
                )
                .run()?;

                // Do not use `git archive --format=tar.gz`: it defers to the
                // machine's `tar.tgz.command` configuration.
                let source_archive = output_dir.join(identity.archive_name());
                let compressed =
                    flowey::shell_cmd!(rt, "gzip -n --best --stdout {source_tar}").output()?;
                fs_err::write(&source_archive, compressed.stdout)?;
                fs_err::remove_file(source_tar)?;

                let archive_name = identity.archive_name();
                rt.sh.change_dir(&output_dir);
                let checksums = flowey::shell_cmd!(rt, "sha256sum {archive_name}").output()?;
                fs_err::write(output_dir.join(CHECKSUM_FILE), checksums.stdout)?;
                fs_err::write(
                    output_dir.join(IDENTITY_FILE),
                    serde_json::to_vec(&identity)?,
                )?;

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

        fs_err::write(&manifest, "[workspace]\nmembers = []\n").unwrap();
        assert!(workspace_version(&manifest).is_err());

        fs_err::write(
            &manifest,
            "[workspace.package]\nversion = { workspace = true }\n",
        )
        .unwrap();
        assert!(workspace_version(&manifest).is_err());

        fs_err::write(&manifest, "[workspace.package]\nversion = \"0.1.0/x\"\n").unwrap();
        assert!(workspace_version(&manifest).is_err());
    }

    #[test]
    fn transfers_identity_outside_the_published_assets() {
        let dir = tempfile::tempdir().unwrap();
        let identity = identity("0.12.3");
        fs_err::write(
            dir.path().join(IDENTITY_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();

        assert_eq!(read_source_identity(dir.path()).unwrap(), identity);
        assert_ne!(IDENTITY_FILE, CHECKSUM_FILE);
        assert_ne!(IDENTITY_FILE, identity.archive_name());
    }
}
