// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assemble the uploaded OpenVMM vendor archive.
//!
//! GitHub provides the source archive for the release tag. This node builds the
//! additional vendor tarball a packager needs for
//! `cargo build --locked --offline`.

use flowey::node::prelude::*;

/// The vendored Cargo source replacement config emitted by `cargo vendor`.
pub const CARGO_CONFIG_FILE: &str = "cargo_config";

/// The `tar` flags that make the vendor archive byte-reproducible.
///
/// Shared with the tests so the tested argument list is the released one. The
/// format is pinned because a vendored Cargo tree has paths past the 100
/// character ustar limit, and the pax format tar picks under `POSIXLY_CORRECT`
/// names its extended headers after the archiving process's pid.
const DETERMINISTIC_TAR_ARGS: &[&str] = &[
    "--sort=name",
    "--mtime=@0",
    "--owner=0",
    "--group=0",
    "--numeric-owner",
    "--mode=u=rwX,go=rX",
    "--format=gnu",
];

/// Internal identity stored alongside the assembled assets.
///
/// This cannot be a [`VendorReleaseOutput`] field: flowey serializes an
/// artifact to JSON and copies every string value as a source path, so a
/// version string would be treated as a file to copy.
pub(crate) const IDENTITY_FILE: &str = ".openvmm-vendor-identity.json";

/// The private identity of the assembled vendor archive.
///
/// Both fields are read out of the tree, so two jobs at the same commit agree.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VendorReleaseIdentity {
    /// The workspace version, e.g. `0.12.3`.
    pub version: String,
    /// The full commit the archive was produced from.
    pub revision: String,
}

/// The assembled vendor archive transferred between jobs.
#[derive(Serialize, Deserialize)]
pub struct VendorReleaseOutput {
    /// Directory containing the vendor archive and internal identity metadata.
    pub assets: PathBuf,
}

impl Artifact for VendorReleaseOutput {}

/// Read the identity transferred with assembled vendor assets.
pub fn read_vendor_identity(assets: &Path) -> anyhow::Result<VendorReleaseIdentity> {
    let path = assets.join(IDENTITY_FILE);
    let contents =
        fs_err::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

impl VendorReleaseIdentity {
    /// The name of the uploaded vendor archive.
    pub fn archive_name(&self) -> String {
        format!("openvmm-{}-vendor.tar.gz", self.version)
    }
}

/// Resolve the identity of the OpenVMM checkout in the current working
/// directory.
pub fn resolve_identity(rt: &mut RustRuntimeServices<'_>) -> anyhow::Result<VendorReleaseIdentity> {
    let revision = flowey::shell_cmd!(rt, "git rev-parse HEAD")
        .read()?
        .trim()
        .to_owned();
    let manifest_path = rt.sh.current_dir().absolute()?.join("Cargo.toml");
    let version = workspace_version(&manifest_path)?;

    Ok(VendorReleaseIdentity { version, revision })
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

/// Confirm `cargo vendor` emitted a source replacement pointing at the relative
/// `vendor` directory.
///
/// The archive ships this config next to the tree it describes, so an absolute
/// path would point a packager at the release machine instead.
fn validate_vendor_config(cargo_config: &[u8]) -> anyhow::Result<()> {
    let cargo_config = std::str::from_utf8(cargo_config)
        .context("cargo vendor emitted a non-UTF-8 source replacement config")?;
    let cargo_config = cargo_config
        .parse::<toml_edit::DocumentMut>()
        .context("failed to parse the source replacement config from cargo vendor")?;

    let directory = cargo_config
        .get("source")
        .and_then(|source| source.get("vendored-sources"))
        .and_then(|vendored| vendored.get("directory"))
        .and_then(|directory| directory.as_str())
        .context("cargo vendor did not emit source.vendored-sources.directory")?;

    if directory != "vendor" {
        anyhow::bail!(
            "cargo vendor emitted source.vendored-sources.directory = {directory:?}, \
             expected the relative path \"vendor\""
        );
    }

    Ok(())
}

flowey_request! {
    pub struct Request {
        /// The assembled vendor assets.
        pub release: WriteVar<VendorReleaseOutput>,
    }
}

/// The directory the vendor assets are assembled into, relative to the job's
/// working directory.
const OUTPUT_DIR: &str = "openvmm-vendor-release";

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { release } = request;

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let rust_is_installed = ctx.reqv(flowey_lib_common::install_rust::Request::EnsureInstalled);
        let rust_toolchain = ctx.reqv(flowey_lib_common::install_rust::Request::GetRustupToolchain);

        ctx.emit_rust_step("assemble OpenVMM vendor archive", |ctx| {
            rust_is_installed.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            let rust_toolchain = rust_toolchain.claim(ctx);
            let release = release.claim(ctx);
            move |rt| {
                let output_dir = std::env::current_dir()?.join(OUTPUT_DIR);
                let repo_path = rt.read(openvmm_repo_path);
                let rust_toolchain = rt.read(rust_toolchain);

                // Assets are named after the version, so a stale archive from a
                // different version would survive reassembly.
                if output_dir.exists() {
                    fs_err::remove_dir_all(&output_dir)?;
                }
                fs_err::create_dir_all(&output_dir)?;

                rt.sh.change_dir(&repo_path);

                let identity = resolve_identity(rt)?;

                // `cargo vendor` resolves against the checkout's manifests and
                // lock file, so tracked modifications would make the identity
                // lie about the bytes in the uploaded archive.
                let dirty =
                    flowey::shell_cmd!(rt, "git status --porcelain --untracked-files=no").read()?;
                if !dirty.trim().is_empty() {
                    anyhow::bail!(
                        "refusing to assemble a vendor archive with tracked modifications; \
                         the archive would not match HEAD.\nmodified:\n{dirty}"
                    );
                }

                let stage_dir = output_dir.join("staging");
                fs_err::create_dir_all(&stage_dir)?;

                let manifest_path = repo_path.join("Cargo.toml");
                let cargo = if let Some(rust_toolchain) = &rust_toolchain {
                    flowey::shell_cmd!(rt, "rustup run {rust_toolchain} cargo")
                } else {
                    flowey::shell_cmd!(rt, "cargo")
                };

                let prior_dir = rt.sh.current_dir();
                rt.sh.change_dir(&stage_dir);
                let vendor_output = cargo
                    .args([
                        "vendor".as_ref(),
                        "--manifest-path".as_ref(),
                        manifest_path.as_os_str(),
                        "--locked".as_ref(),
                        "--versioned-dirs".as_ref(),
                        "vendor".as_ref(),
                    ])
                    .ignore_status()
                    .output()?;
                rt.sh.change_dir(prior_dir);

                if !vendor_output.status.success() {
                    anyhow::bail!(
                        "cargo vendor failed with {}.\nstderr:\n{}",
                        vendor_output.status,
                        String::from_utf8_lossy(&vendor_output.stderr)
                    );
                }

                let cargo_config = vendor_output.stdout;
                validate_vendor_config(&cargo_config)?;

                fs_err::write(stage_dir.join(CARGO_CONFIG_FILE), &cargo_config)?;

                let vendor_dir = stage_dir.join("vendor");
                if !vendor_dir.is_dir() {
                    anyhow::bail!(
                        "cargo vendor did not produce the expected directory {}",
                        vendor_dir.display()
                    );
                }

                let tar_path = output_dir.join(identity.archive_name()).with_extension("");
                let cargo_config_file = CARGO_CONFIG_FILE;
                let tar_args = DETERMINISTIC_TAR_ARGS;
                rt.sh.change_dir(&stage_dir);
                flowey::shell_cmd!(
                    rt,
                    "tar {tar_args...} -cf {tar_path} vendor {cargo_config_file}"
                )
                .run()?;
                flowey::shell_cmd!(rt, "gzip -n --best -f {tar_path}").run()?;

                rt.sh.change_dir(&output_dir);
                fs_err::remove_dir_all(&stage_dir)?;
                fs_err::write(
                    output_dir.join(IDENTITY_FILE),
                    serde_json::to_vec(&identity)?,
                )?;

                rt.write(release, &VendorReleaseOutput { assets: output_dir });

                Ok(())
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(version: &str) -> VendorReleaseIdentity {
        VendorReleaseIdentity {
            version: version.into(),
            revision: "0123456789abcdef0123456789abcdef01234567".into(),
        }
    }

    #[test]
    fn asset_names_follow_the_version() {
        let identity = identity("0.12.3");
        assert_eq!(identity.archive_name(), "openvmm-0.12.3-vendor.tar.gz");
    }

    #[test]
    fn accepts_a_relative_vendor_source_replacement() {
        // Formatting varies across Cargo versions, so the check must be
        // structural rather than textual.
        let config = b"[source.crates-io]\nreplace-with = 'vendored-sources'\n\n\
            [source.vendored-sources]\ndirectory   =   \"vendor\"\n";
        validate_vendor_config(config).unwrap();
    }

    #[test]
    fn rejects_an_absolute_or_missing_vendor_directory() {
        // An absolute path would point back at the release machine.
        let absolute = b"[source.vendored-sources]\ndirectory = \"/build/stage/vendor\"\n";
        assert!(validate_vendor_config(absolute).is_err());

        let missing = b"[source.crates-io]\nreplace-with = \"vendored-sources\"\n";
        assert!(validate_vendor_config(missing).is_err());

        assert!(validate_vendor_config(b"not = = toml").is_err());
        assert!(validate_vendor_config(&[0xff, 0xfe]).is_err());
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

    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;
        use std::collections::BTreeSet;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        fn run_command(
            mut command: Command,
            description: &str,
        ) -> anyhow::Result<std::process::Output> {
            let output = command
                .output()
                .with_context(|| format!("failed to run {description}"))?;
            if output.status.success() {
                return Ok(output);
            }

            anyhow::bail!(
                "{description} failed with {}.\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        fn create_deterministic_vendor_archive(
            stage_dir: &Path,
            archive_path: &Path,
        ) -> anyhow::Result<()> {
            let tar_path = archive_path.with_extension("");

            let mut tar = Command::new("tar");
            tar.current_dir(stage_dir)
                .args(DETERMINISTIC_TAR_ARGS)
                .arg("-cf")
                .arg(&tar_path)
                .args(["vendor", CARGO_CONFIG_FILE]);
            run_command(tar, "tar")?;

            let mut gzip = Command::new("gzip");
            gzip.args(["-n", "--best", "-f"]).arg(&tar_path);
            run_command(gzip, "gzip")?;

            anyhow::ensure!(
                archive_path.is_file(),
                "gzip did not produce {}",
                archive_path.display()
            );
            Ok(())
        }

        fn write_fixture(stage_dir: &Path) {
            let crate_dir = stage_dir.join("vendor").join("demo-0.1.0");
            fs_err::create_dir_all(crate_dir.join("bin")).unwrap();
            fs_err::write(
                stage_dir.join(CARGO_CONFIG_FILE),
                b"[source.crates-io]\nreplace-with = \"vendored-sources\"\n[source.vendored-sources]\ndirectory = \"vendor\"\n",
            )
            .unwrap();
            fs_err::write(crate_dir.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
            fs_err::write(crate_dir.join("bin").join("tool"), b"#!/bin/sh\nexit 0\n").unwrap();
            fs_err::set_permissions(
                crate_dir.join("bin").join("tool"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();

            // Longer than the 100 character ustar path limit, so the archive
            // has to carry the name in a format extension. Without this a
            // fixture cannot detect a format whose extended headers are named
            // nondeterministically.
            let long_dir = crate_dir
                .join("src")
                .join("a".repeat(60))
                .join("b".repeat(60));
            fs_err::create_dir_all(&long_dir).unwrap();
            fs_err::write(long_dir.join("deeply_nested_source.rs"), b"// vendored\n").unwrap();
        }

        fn set_timestamp(path: &Path, timestamp: &str) {
            let mut touch = Command::new("touch");
            touch.args(["-d", timestamp]).arg(path);
            run_command(touch, "touch").unwrap();
        }

        fn list_archive_entries(archive: &Path) -> Vec<String> {
            let mut tar = Command::new("tar");
            tar.args(["-tzf"]).arg(archive);
            let output = run_command(tar, "tar -tzf").unwrap();
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        #[test]
        fn archive_has_the_expected_top_level_layout() {
            let dir = tempfile::tempdir().unwrap();
            let stage_dir = dir.path().join("stage");
            fs_err::create_dir_all(&stage_dir).unwrap();
            write_fixture(&stage_dir);

            let archive = dir.path().join("openvmm-0.12.3-vendor.tar.gz");
            create_deterministic_vendor_archive(&stage_dir, &archive).unwrap();

            let entries = list_archive_entries(&archive);
            let top_level = entries
                .iter()
                .map(|entry| {
                    entry
                        .trim_end_matches('/')
                        .split('/')
                        .next()
                        .unwrap()
                        .to_owned()
                })
                .collect::<BTreeSet<_>>();

            assert_eq!(
                top_level,
                BTreeSet::from(["cargo_config".to_owned(), "vendor".to_owned()])
            );
            assert!(entries.iter().any(|entry| entry == CARGO_CONFIG_FILE));
            assert!(entries.iter().any(|entry| entry == "vendor/"));
        }

        #[test]
        fn deterministic_tiny_fixture_tar_output_on_linux() {
            let dir = tempfile::tempdir().unwrap();

            let first_stage = dir.path().join("stage-a");
            let second_stage = dir.path().join("stage-b");
            fs_err::create_dir_all(&first_stage).unwrap();
            fs_err::create_dir_all(&second_stage).unwrap();

            write_fixture(&first_stage);
            write_fixture(&second_stage);

            set_timestamp(
                &first_stage
                    .join("vendor")
                    .join("demo-0.1.0")
                    .join("Cargo.toml"),
                "2001-02-03 04:05:06 UTC",
            );
            set_timestamp(
                &second_stage
                    .join("vendor")
                    .join("demo-0.1.0")
                    .join("Cargo.toml"),
                "2011-12-13 14:15:16 UTC",
            );

            let first_archive = dir.path().join("first.tar.gz");
            let second_archive = dir.path().join("second.tar.gz");
            create_deterministic_vendor_archive(&first_stage, &first_archive).unwrap();
            create_deterministic_vendor_archive(&second_stage, &second_archive).unwrap();

            assert_eq!(
                fs_err::read(first_archive).unwrap(),
                fs_err::read(second_archive).unwrap()
            );
        }
    }
}
