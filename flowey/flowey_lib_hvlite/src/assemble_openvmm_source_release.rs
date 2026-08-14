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
//! The identity is derived from the checkout being archived rather than
//! supplied by the caller, so it cannot describe a tree other than the one
//! exported, and callers need no preparation step to use this node.
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
    ///
    /// This is `<name>-<version>.tar.gz`, matching the root directory. That
    /// pairing is what distribution tooling assumes by default: RPM's
    /// `%autosetup` unpacks the source and enters `%{name}-%{version}`, and
    /// Fedora's forge macros derive this filename. Naming the file anything
    /// else would still package, but every spec file would have to override
    /// the default to say so.
    pub fn archive_name(&self) -> String {
        format!("{}.tar.gz", self.source_root())
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
        /// The assembled source assets.
        pub release: WriteVar<SourceReleaseOutput>,
    }
}

/// The directory the source assets are assembled into, relative to the job's
/// working directory.
const OUTPUT_DIR: &str = "openvmm-source-release";

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { release } = request;

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        ctx.emit_rust_step("assemble OpenVMM source archive", |ctx| {
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            let release = release.claim(ctx);
            move |rt| {
                let output_dir = std::env::current_dir()?.join(OUTPUT_DIR);
                let repo_path = rt.read(openvmm_repo_path);

                // Assets are named after the version, so a stale archive from a
                // different version would survive reassembly and ride along
                // into whatever publishes this directory.
                if output_dir.exists() {
                    fs_err::remove_dir_all(&output_dir)?;
                }
                fs_err::create_dir_all(&output_dir)?;
                rt.sh.change_dir(&repo_path);

                // Derived from the checkout being archived, so the identity
                // cannot describe a different tree than the one exported.
                let identity = resolve_identity(rt)?;

                // `git archive` exports the tree at HEAD, so an uncommitted
                // change would silently not appear in the archive. Untracked
                // files are excluded from the archive by design and so are
                // deliberately not checked here.
                let dirty =
                    flowey::shell_cmd!(rt, "git status --porcelain --untracked-files=no").read()?;
                if !dirty.trim().is_empty() {
                    anyhow::bail!(
                        "refusing to assemble a source archive with tracked modifications; \
                         the archive would not match HEAD.\nmodified:\n{dirty}"
                    );
                }

                // Pin the mode mask rather than inheriting machine-specific
                // Git configuration.
                //
                // The intermediate tar is named so that `gzip` produces
                // exactly `archive_name()`, since `gzip` derives its output
                // name by appending `.gz`.
                let prefix = format!("{}/", identity.source_root());
                let source_tar = output_dir.join(format!("{}.tar", identity.source_root()));
                flowey::shell_cmd!(
                    rt,
                    "git -c tar.umask=0002 archive --format=tar --output {source_tar} --prefix={prefix} HEAD"
                )
                .run()?;

                // Do not use `git archive --format=tar.gz`: it defers to the
                // machine's `tar.tgz.command` configuration.
                //
                // `gzip` replaces the tar in place rather than writing to
                // stdout, so the archive is never buffered in this process.
                // `-f` because `gzip` otherwise refuses to replace an archive
                // left behind by an earlier run in the same directory.
                flowey::shell_cmd!(rt, "gzip -n --best -f {source_tar}").run()?;

                let source_archive = output_dir.join(identity.archive_name());
                if !source_archive.exists() {
                    anyhow::bail!(
                        "gzip did not produce the expected archive {}",
                        source_archive.display()
                    );
                }

                let archive_name = identity.archive_name();
                rt.sh.change_dir(&output_dir);
                let checksums = flowey::shell_cmd!(rt, "sha256sum {archive_name}").output()?;
                fs_err::write(output_dir.join(CHECKSUM_FILE), checksums.stdout)?;
                fs_err::write(
                    output_dir.join(IDENTITY_FILE),
                    serde_json::to_vec(&identity)?,
                )?;

                rt.write(release, &SourceReleaseOutput { assets: output_dir });

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
        assert_eq!(identity.archive_name(), "openvmm-0.12.3.tar.gz");
    }

    /// Distribution tooling defaults to entering `<name>-<version>` after
    /// unpacking `<name>-<version>.tar.gz`, so the two must agree.
    #[test]
    fn archive_name_is_the_source_root_plus_an_extension() {
        let identity = identity("0.12.3");
        assert_eq!(
            identity.archive_name(),
            format!("{}.tar.gz", identity.source_root())
        );
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
