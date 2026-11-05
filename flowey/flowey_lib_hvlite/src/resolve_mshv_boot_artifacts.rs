// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolve pinned public mshv payloads or use an explicitly supplied directory.

use crate::common::CommonArch;
use flowey::node::prelude::*;
use sha2::Digest as _;
use std::collections::BTreeMap;
use std::io::Read as _;
use std::io::Write as _;

/// Extracted MSHV payloads, including packaged licenses and source hashes.
#[derive(Serialize, Deserialize)]
pub struct MshvBootArtifactsOutput {
    #[serde(rename = "mshv-boot-artifacts")]
    pub directory: PathBuf,
}

impl Artifact for MshvBootArtifactsOutput {}

flowey_config! {
    pub struct Config {
        /// Authorized local artifacts, containing boot/HvLoader.efi and its runtime.
        pub local_path: Option<ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub struct Request {
        pub arch: CommonArch,
        pub artifacts: WriteVar<MshvBootArtifactsOutput>,
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let Config { local_path } = config;
        let mut outputs = Vec::new();
        for Request { arch, artifacts } in requests {
            anyhow::ensure!(
                arch == CommonArch::X86_64,
                "mshv payloads support only x86-64"
            );
            outputs.push(artifacts);
        }
        if outputs.is_empty() {
            return Ok(());
        }

        anyhow::ensure!(
            local_path.is_some() || matches!(ctx.platform(), FlowPlatform::Linux(_)),
            "automatic mshv RPM acquisition requires Linux; supply local boot artifacts instead"
        );

        let installed = if local_path.is_none()
            && !matches!(
                ctx.platform(),
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Nix)
            ) {
            let package_names = match ctx.platform() {
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu) => {
                    vec!["curl", "rpm2cpio", "libarchive-tools"]
                }
                FlowPlatform::Linux(FlowPlatformLinuxDistro::Fedora) => {
                    vec!["curl", "rpm", "bsdtar"]
                }
                FlowPlatform::Linux(
                    FlowPlatformLinuxDistro::AzureLinux | FlowPlatformLinuxDistro::Arch,
                ) => {
                    vec!["curl", "rpm", "libarchive"]
                }
                _ => vec!["curl", "rpm2cpio", "bsdtar"],
            }
            .into_iter()
            .map(String::from)
            .collect();
            Some(ctx.reqv(
                |done| flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names,
                    done,
                },
            ))
        } else {
            None
        };
        let persistent_dir = ctx.persistent_dir();
        ctx.emit_rust_step("resolve mshv boot payloads", |ctx| {
            let local_path = local_path.claim(ctx);
            let persistent_dir = persistent_dir.claim(ctx);
            let outputs = outputs.claim(ctx);
            installed.claim(ctx);
            move |rt| {
                let path = if let Some(local_path) = local_path {
                    rt.read(local_path)
                } else {
                    let cache = persistent_dir
                        .map(|dir| rt.read(dir))
                        .unwrap_or_else(|| rt.sh.current_dir())
                        .join("mshv");
                    acquire(rt, &cache)?
                };
                validate_artifacts(&path)?;
                let path = path.absolute()?;
                rt.write_all(outputs, &MshvBootArtifactsOutput { directory: path });
                Ok(())
            }
        });
        Ok(())
    }
}

fn validate_artifacts(root: &Path) -> anyhow::Result<()> {
    for file in [
        "boot/HvLoader.efi",
        "boot/lxhvloader.dll",
        "boot/Windows/System32/hvix64.exe",
        "boot/Windows/System32/hvax64.exe",
    ] {
        anyhow::ensure!(
            root.join(file).is_file(),
            "missing mshv boot artifact {}",
            root.join(file).display()
        );
    }
    Ok(())
}

const REPOSITORY: &str = "https://packages.microsoft.com/azurelinux/3.0/prod";
const RECIPE: &str = "public-26100.9444";

struct Package {
    repository: &'static str,
    filename: &'static str,
    sha256: &'static str,
}

impl Package {
    fn url(&self) -> String {
        format!(
            "{}/{}/x86_64/Packages/{}/{}",
            REPOSITORY,
            self.repository,
            &self.filename[..1],
            self.filename
        )
    }
}

const PACKAGES: &[Package] = &[
    Package {
        repository: "base",
        filename: "edk2-hvloader-20240524git3e722403cd16-20.azl3.x86_64.rpm",
        sha256: "8f5dcf56e7a009f5e2a9f594c46c6d6b43345eff7f2ddd4b011095ab95ec40c3",
    },
    Package {
        repository: "ms-non-oss",
        filename: "mshv-26100.9444.2609032029.1-1.azl3.x86_64.rpm",
        sha256: "95a95671546fc29488ffd8b61c00d9869928ceca25005f5c1ca3beaaf83d9385",
    },
    Package {
        repository: "ms-non-oss",
        filename: "mshv-bootloader-lx-26100.9444.2609032029.1-1.azl3.x86_64.rpm",
        sha256: "cf75cc2633df5d35484ea75aacea7d308a1b18f4064c0e159d99e0e20c48df63",
    },
];

fn verify_hash(path: &Path, expected: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        fs_err::symlink_metadata(path)?.file_type().is_file(),
        "RPM cache entry is not a regular file: {}",
        path.display()
    );
    let mut file = fs_err::File::open(path)?;
    let mut hash = sha2::Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let len = file.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        hash.update(&buffer[..len]);
    }
    let actual = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    anyhow::ensure!(
        actual == expected,
        "SHA-256 mismatch for {}: expected {expected}, found {actual}",
        path.display()
    );
    Ok(())
}

fn cached_rpm(
    cache: &Path,
    package: &Package,
    download: &mut impl FnMut(&str, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<PathBuf> {
    let path = cache.join(package.filename);
    match fs_err::symlink_metadata(&path) {
        Ok(_) => {
            verify_hash(&path, package.sha256)?;
            return Ok(path);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let temp = tempfile::NamedTempFile::new_in(cache)?;
    download(&package.url(), temp.path())?;
    verify_hash(temp.path(), package.sha256)?;
    match temp.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            log::debug!("another writer populated RPM cache {}", path.display());
            verify_hash(&path, package.sha256)?;
        }
        Err(error) => return Err(error.error.into()),
    }
    Ok(path)
}

fn acquire(rt: &mut RustRuntimeServices<'_>, cache: &Path) -> anyhow::Result<PathBuf> {
    fs_err::create_dir_all(cache)?;
    let mut files = BTreeMap::new();
    let mut manifest = String::new();
    for package in PACKAGES {
        let path = cached_rpm(cache, package, &mut |url, output| {
            flowey::shell_cmd!(
                rt,
                "curl --fail --location --proto =https --proto-redir =https
                     --connect-timeout 30 --max-time 300 --output {output} {url}"
            )
            .run()?;
            Ok(())
        })?;
        let bsdtar = flowey_lib_common::_util::bsdtar_name(rt);
        for (path, contents) in extract_rpm(&rt.sh, bsdtar, &path)? {
            anyhow::ensure!(
                !files
                    .keys()
                    .any(|key: &String| key.eq_ignore_ascii_case(&path)),
                "duplicate extracted path: {path}"
            );
            files.insert(path, contents);
        }
        manifest.push_str(&format!("{}  {}\n", package.sha256, package.url()));
    }
    files.insert("SHA256SUMS".into(), manifest.into_bytes());
    let output = cache.join(RECIPE);
    if !output.exists() {
        let scratch = tempfile::Builder::new()
            .prefix(".mshv-extract-")
            .tempdir_in(cache)?;
        for (name, contents) in &files {
            let path = scratch.path().join(name);
            fs_err::create_dir_all(path.parent().context("missing output parent")?)?;
            let mut file = fs_err::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            file.write_all(contents)?;
        }
        validate_artifacts(scratch.path())?;
        match fs_err::rename(scratch.path(), &output) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                log::debug!(
                    "another writer populated mshv payloads {}",
                    output.display()
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    verify_output(&output, &files)?;
    Ok(output)
}

fn archive_members(listing: &str) -> anyhow::Result<Vec<String>> {
    let mut members = Vec::new();
    for entry in listing.lines() {
        let relative = entry
            .strip_prefix("./")
            .unwrap_or(entry)
            .trim_start_matches('/')
            .trim_end_matches('/');
        if relative.is_empty() {
            continue;
        }
        if destination(&format!("/{relative}"))?.is_some() {
            members.push(entry.to_owned());
        }
    }
    Ok(members)
}

fn extract_rpm(
    sh: &flowey::shell::FloweyShell,
    bsdtar: &str,
    rpm: &Path,
) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let archive = tempfile::NamedTempFile::new()?;
    let bytes = sh
        .wrap(sh.cmd("rpm2cpio"))
        .arg(rpm)
        .output()
        .context(
            "local RPM preparation requires rpm2cpio; install it or supply pre-extracted payloads \
             with --mshv-boot-artifacts (vmm-tests-run) or --boot-artifacts (Incubator)",
        )?
        .stdout;
    fs_err::write(archive.path(), bytes)?;
    let archive = archive.path();
    let listing = sh.wrap(sh.cmd(bsdtar)).arg("-tf").arg(archive).read()?;
    let members = archive_members(&listing)?;
    anyhow::ensure!(
        !members.is_empty(),
        "RPM contains no boot payloads or licenses: {}",
        rpm.display()
    );
    let scratch = tempfile::tempdir()?;
    let root = scratch.path();
    sh.wrap(sh.cmd(bsdtar))
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(root)
        .args(["--no-same-owner", "--no-same-permissions", "--"])
        .args(members)
        .run()?;
    read_extracted(root)
}

fn read_extracted(root: &Path) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(dir) = directories.pop() {
        for entry in fs_err::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(path);
                continue;
            }
            anyhow::ensure!(
                kind.is_file(),
                "RPM entry is not a regular file: {}",
                path.display()
            );
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .context("non-UTF-8 RPM path")?;
            let name = destination(&format!("/{relative}"))?
                .with_context(|| format!("unexpected extracted RPM path: {relative}"))?;
            anyhow::ensure!(
                !files
                    .keys()
                    .any(|key: &String| key.eq_ignore_ascii_case(&name)),
                "duplicate extracted path: {name}"
            );
            files.insert(name, fs_err::read(path)?);
        }
    }
    Ok(files)
}

fn verify_output(root: &Path, files: &BTreeMap<String, Vec<u8>>) -> anyhow::Result<()> {
    anyhow::ensure!(
        fs_err::symlink_metadata(root)?.file_type().is_dir(),
        "mshv payload cache is not a directory: {}",
        root.display()
    );
    let mut directories = vec![root.to_path_buf()];
    let mut found = 0;
    while let Some(dir) = directories.pop() {
        for entry in fs_err::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(path);
                continue;
            }
            let relative = path.strip_prefix(root)?;
            anyhow::ensure!(
                kind.is_file() && files.keys().any(|name| Path::new(name) == relative),
                "unexpected mshv payload cache entry: {}",
                path.display()
            );
            found += 1;
        }
    }
    anyhow::ensure!(
        found == files.len(),
        "incomplete mshv payload cache: {}",
        root.display()
    );
    for (name, contents) in files {
        let path = root.join(name);
        anyhow::ensure!(
            fs_err::symlink_metadata(&path)?.file_type().is_file(),
            "cached mshv payload is not a regular file: {}",
            path.display()
        );
        anyhow::ensure!(
            fs_err::read(&path)? == *contents,
            "cached mshv payload does not match its pinned RPM: {}",
            path.display()
        );
    }
    Ok(())
}

fn destination(path: &str) -> anyhow::Result<Option<String>> {
    anyhow::ensure!(
        path.starts_with('/')
            && path[1..].split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part.is_ascii()
                    && !part.ends_with([' ', '.'])
                    && !part
                        .chars()
                        .any(|c| c.is_control() || "\"*:<>?\\|".contains(c))
            }),
        "unsupported RPM path: {path:?}"
    );
    if let Some(path) = path.strip_prefix("/boot/efi/") {
        Ok(Some(format!("boot/{path}")))
    } else {
        Ok(path
            .strip_prefix("/usr/share/licenses/")
            .map(|path| format!("licenses/{path}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    #[ignore = "requires rpm2cpio, bsdtar, and pinned RPMs in MSHV_RPM_DIR"]
    fn extracts_pinned_rpm_payloads() -> anyhow::Result<()> {
        let root = PathBuf::from(
            std::env::var_os("MSHV_RPM_DIR")
                .context("set MSHV_RPM_DIR to the pinned RPM directory")?,
        );
        let sh = flowey::shell::FloweyShell::new()?;
        let mut files = BTreeMap::new();
        for package in PACKAGES {
            let path = root.join(package.filename);
            verify_hash(&path, package.sha256)?;
            for (name, data) in extract_rpm(&sh, "bsdtar", &path)? {
                assert!(files.insert(name, data).is_none());
            }
        }
        for path in [
            "boot/HvLoader.efi",
            "boot/lxhvloader.dll",
            "boot/Windows/System32/hvix64.exe",
            "boot/Windows/System32/hvax64.exe",
        ] {
            assert!(
                files
                    .get(path)
                    .context("missing boot payload")?
                    .starts_with(b"MZ"),
                "{path}"
            );
        }
        assert!(files.keys().any(|path| path.starts_with("licenses/")));
        Ok(())
    }

    #[test]
    fn archive_selection_validates_paths_before_extraction() -> anyhow::Result<()> {
        assert_eq!(
            archive_members(
                "./boot/efi/HvLoader.efi\n./usr/share/licenses/mshv/EULA.txt\n./etc/config\n"
            )?,
            [
                "./boot/efi/HvLoader.efi",
                "./usr/share/licenses/mshv/EULA.txt"
            ]
        );
        for listing in [
            "./boot/efi/../escape\n",
            "/boot/efi/a\\b\n",
            "./boot/efi/file.\n",
        ] {
            assert!(archive_members(listing).is_err());
        }
        Ok(())
    }

    #[test]
    fn extracted_files_keep_licenses_and_reject_case_collisions() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        fs_err::create_dir_all(root.path().join("boot/efi"))?;
        fs_err::create_dir_all(root.path().join("usr/share/licenses/mshv"))?;
        fs_err::write(root.path().join("boot/efi/HvLoader.efi"), b"loader")?;
        fs_err::write(
            root.path().join("usr/share/licenses/mshv/EULA.txt"),
            b"license",
        )?;
        let files = read_extracted(root.path())?;
        assert_eq!(files["boot/HvLoader.efi"], b"loader");
        assert_eq!(files["licenses/mshv/EULA.txt"], b"license");
        fs_err::write(root.path().join("boot/efi/HVLOADER.EFI"), b"collision")?;
        if fs_err::read_to_string(root.path().join("boot/efi/HvLoader.efi"))? == "loader" {
            assert!(read_extracted(root.path()).is_err());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn extracted_symlinks_are_rejected() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        fs_err::create_dir_all(root.path().join("boot/efi"))?;
        std::os::unix::fs::symlink("/outside", root.path().join("boot/efi/HvLoader.efi"))?;
        assert!(read_extracted(root.path()).is_err());
        Ok(())
    }

    #[test]
    fn verifies_cached_rpms_without_downloading_again() -> anyhow::Result<()> {
        let cache = tempfile::tempdir()?;
        let package = Package {
            repository: "base",
            filename: "test.rpm",
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        let mut unexpected_download =
            |_: &str, _: &Path| -> anyhow::Result<()> { anyhow::bail!("unexpected download") };
        let path = cache.path().join(package.filename);
        fs_err::write(&path, b"abc")?;
        assert_eq!(
            cached_rpm(cache.path(), &package, &mut unexpected_download)?,
            path
        );
        fs_err::write(&path, b"bad")?;
        assert!(cached_rpm(cache.path(), &package, &mut unexpected_download).is_err());
        Ok(())
    }

    #[test]
    fn rejects_unsafe_paths_and_preserves_licenses() -> anyhow::Result<()> {
        assert_eq!(
            destination("/boot/efi/HvLoader.efi")?,
            Some("boot/HvLoader.efi".into())
        );
        assert_eq!(
            destination("/usr/share/licenses/mshv/EULA.txt")?,
            Some("licenses/mshv/EULA.txt".into())
        );
        assert_eq!(destination("/etc/config")?, None);
        for path in [
            "/boot/efi/../escape",
            "/boot/efi//file",
            "/boot/efi/a\\b",
            "/boot/efi/C:drive",
            "/boot/efi/file.",
            "/boot/efi/x\n",
        ] {
            assert!(destination(path).is_err(), "{path:?}");
        }
        Ok(())
    }

    #[test]
    fn rejects_modified_extracted_payloads() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let expected = BTreeMap::from([("HvLoader.efi".into(), b"pinned".to_vec())]);
        fs_err::write(root.path().join("HvLoader.efi"), b"pinned")?;
        verify_output(root.path(), &expected)?;
        fs_err::write(root.path().join("HvLoader.efi"), b"modified")?;
        assert!(verify_output(root.path(), &expected).is_err());
        fs_err::write(root.path().join("HvLoader.efi"), b"pinned")?;
        fs_err::write(root.path().join("unverified.dll"), b"extra")?;
        assert!(verify_output(root.path(), &expected).is_err());
        Ok(())
    }

    #[test]
    fn publishes_only_verified_downloads() -> anyhow::Result<()> {
        let cache = tempfile::tempdir()?;
        let package = Package {
            repository: "base",
            filename: "test.rpm",
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        assert!(
            cached_rpm(cache.path(), &package, &mut |_, path| {
                fs_err::write(path, b"bad")?;
                Ok(())
            })
            .is_err()
        );
        assert!(!cache.path().join(package.filename).exists());
        let path = cached_rpm(cache.path(), &package, &mut |_, path| {
            fs_err::write(path, b"abc")?;
            Ok(())
        })?;
        assert_eq!(fs_err::read(path)?, b"abc");
        Ok(())
    }
}
