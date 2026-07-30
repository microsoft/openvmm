// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assemble the OpenVMM source release.
//!
//! This produces the files a source release consists of: a `.tar.gz` of the
//! tracked source at a single revision, and a `SHA256SUMS` covering it. The
//! archive carries its identity in a `.openvmm-release.json` file, because a
//! packager building it has no `.git` directory to recover a version from.
//!
//! Assembly is reproducible: `git archive` emits a deterministic tar for a
//! given commit, and `gzip -n` omits the timestamp that would otherwise vary.
//! Two jobs handed the same [`SourceIdentity`] at the same commit therefore
//! produce the same bytes, so a consumer of this node never has to be handed an
//! archive to be sure it has the same one.

use flowey::node::prelude::*;

/// Checksums covering every assembled asset.
pub const CHECKSUM_FILE: &str = "SHA256SUMS";

/// Release identity, carried inside the archive itself.
pub const METADATA_FILE: &str = ".openvmm-release.json";

const METADATA_SCHEMA_VERSION: u32 = 1;

/// The identity of a source release.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SourceIdentity {
    /// Three-component version, e.g. `0.12.3`.
    pub version: String,
    /// The release tag, absent for an archive that is not a release.
    pub tag: Option<String>,
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
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
struct ReleaseMetadata {
    schema_version: u32,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    revision: String,
}

impl From<&SourceIdentity> for ReleaseMetadata {
    fn from(identity: &SourceIdentity) -> Self {
        Self {
            schema_version: METADATA_SCHEMA_VERSION,
            version: identity.version.clone(),
            tag: identity.tag.clone(),
            revision: identity.revision.clone(),
        }
    }
}

flowey_request! {
    pub struct Request {
        /// Identity to embed in the release.
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

                // Staged outside the archive and injected by `git archive`,
                // which keeps assembly to a single pass over the tree.
                let metadata_dir = output_dir.join("metadata");
                if metadata_dir.exists() {
                    fs_err::remove_dir_all(&metadata_dir)?;
                }
                fs_err::create_dir_all(&metadata_dir)?;
                let metadata_path = metadata_dir.join(METADATA_FILE);
                let mut metadata_json =
                    serde_json::to_vec_pretty(&ReleaseMetadata::from(&identity))?;
                metadata_json.push(b'\n');
                fs_err::write(&metadata_path, metadata_json)?;

                // `--add-file` places the file at the basename, under the
                // preceding `--prefix`, so the ordering here matters.
                //
                // `tar.umask` masks the mode of every entry, so pin it rather
                // than inheriting whatever the machine has configured. This is
                // the same class of hazard as `tar.tgz.command` below.
                let prefix = format!("{}/", identity.source_root());
                let source_tar = output_dir.join("openvmm-source.tar");
                flowey::shell_cmd!(
                    rt,
                    "git -c tar.umask=0002 archive --format=tar --output {source_tar} --prefix={prefix} --add-file={metadata_path} HEAD"
                )
                .run()?;
                fs_err::remove_dir_all(&metadata_dir)?;

                // `-n` omits the timestamp and original name from the gzip
                // header, which is what makes the result reproducible. Do not
                // use `git archive --format=tar.gz`: it defers to the
                // `tar.tgz.command` config, so reproducibility would depend on
                // the machine's git configuration.
                let source_archive = output_dir.join(identity.archive_name());
                let compressed = flowey::shell_cmd!(rt, "gzip -n --best --stdout {source_tar}")
                    .output()?;
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

/// The metadata an archive assembled under `identity` declares.
pub fn expected_metadata(identity: &SourceIdentity) -> serde_json::Value {
    serde_json::to_value(ReleaseMetadata::from(identity)).expect("release metadata is plain data")
}

/// Which identity a source release is assembled under.
///
/// A commit under test is not a release, so it is assembled under a version
/// that cannot be mistaken for one, with no tag. Everything else about the
/// assembly is what a release does.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentitySource {
    /// A commit under test, which is not a release.
    Snapshot,
}

/// Version used for an archive that is not a release.
const SNAPSHOT_VERSION: &str = "0.0.0-dev";

impl IdentitySource {
    /// Resolve the identity to assemble under.
    ///
    /// The working directory must already be the OpenVMM repository.
    pub fn resolve(self, rt: &mut RustRuntimeServices<'_>) -> anyhow::Result<SourceIdentity> {
        let revision = flowey::shell_cmd!(rt, "git rev-parse HEAD").read()?;

        let (version, tag) = match self {
            IdentitySource::Snapshot => (SNAPSHOT_VERSION.to_owned(), None),
        };

        Ok(SourceIdentity {
            version,
            tag,
            revision,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(version: &str, tag: Option<&str>) -> SourceIdentity {
        SourceIdentity {
            version: version.into(),
            tag: tag.map(Into::into),
            revision: "0123456789abcdef0123456789abcdef01234567".into(),
        }
    }

    #[test]
    fn asset_names_follow_the_version() {
        let identity = identity("0.12.3", Some("openvmm-v0.12.3"));
        assert_eq!(identity.source_root(), "openvmm-0.12.3");
        assert_eq!(identity.archive_name(), "openvmm-0.12.3-source.tar.gz");
    }

    #[test]
    fn release_metadata_matches_source_bundle_schema() {
        let metadata = ReleaseMetadata::from(&identity("0.12.3", Some("openvmm-v0.12.3")));
        assert_eq!(
            serde_json::to_value(metadata).unwrap(),
            serde_json::json!({
                "schema_version": 1,
                "version": "0.12.3",
                "tag": "openvmm-v0.12.3",
                "revision": "0123456789abcdef0123456789abcdef01234567",
            })
        );
    }

    #[test]
    fn untagged_metadata_omits_the_tag() {
        let metadata = ReleaseMetadata::from(&identity("0.0.0-dev", None));
        assert_eq!(
            serde_json::to_value(metadata).unwrap(),
            serde_json::json!({
                "schema_version": 1,
                "version": "0.0.0-dev",
                "revision": "0123456789abcdef0123456789abcdef01234567",
            })
        );
    }
}
