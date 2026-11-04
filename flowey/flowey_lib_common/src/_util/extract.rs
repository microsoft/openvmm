// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;

const FLOWEY_INFO_DIR: &str = ".flowey_info";
const FLOWEY_EXTRACT_DIR: &str = "extracted";

#[derive(Clone)]
#[non_exhaustive]
pub struct ExtractZipDeps<C = VarNotClaimed> {
    persistent_dir: Option<ReadVar<PathBuf, C>>,
    bsdtar_installed: ReadVar<SideEffect, C>,
}

impl ClaimVar for ExtractZipDeps {
    type Claimed = ExtractZipDeps<VarClaimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Self::Claimed {
        let Self {
            persistent_dir,
            bsdtar_installed,
        } = self;
        ExtractZipDeps {
            persistent_dir: persistent_dir.claim(ctx),
            bsdtar_installed: bsdtar_installed.claim(ctx),
        }
    }
}

#[track_caller]
pub fn extract_zip_if_new_deps(ctx: &mut NodeCtx<'_>) -> ExtractZipDeps {
    let platform = ctx.platform();
    ExtractZipDeps {
        persistent_dir: ctx.persistent_dir(),
        bsdtar_installed: ctx.reqv(|v| crate::install_dist_pkg::Request::Install {
            package_names: match platform {
                FlowPlatform::Linux(linux_distribution) => match linux_distribution {
                    FlowPlatformLinuxDistro::Fedora => {
                        vec!["bsdtar".into()]
                    }
                    FlowPlatformLinuxDistro::Ubuntu => vec!["libarchive-tools".into()],
                    FlowPlatformLinuxDistro::AzureLinux | FlowPlatformLinuxDistro::Arch => {
                        vec!["libarchive".into()]
                    }
                    FlowPlatformLinuxDistro::Nix => vec![],
                    FlowPlatformLinuxDistro::Unknown => vec![],
                },
                _ => {
                    vec![]
                }
            },
            done: v,
        }),
    }
}

/// Extracts the given `file` into `persistent_dir` (or into
/// [`std::env::current_dir()`], if no persistent dir is available).
///
/// To avoid redundant unzips between pipeline runs, callers must provide a
/// `file_version` string that identifies the current file. If the
/// previous run already unzipped a zip with the given `file_version`, this
/// function will return nearly instantaneously.
pub fn extract_zip_if_new(
    rt: &mut RustRuntimeServices<'_>,
    deps: ExtractZipDeps<VarClaimed>,
    file: &Path,
    file_version: &str,
) -> anyhow::Result<PathBuf> {
    let ExtractZipDeps {
        persistent_dir,
        bsdtar_installed: _,
    } = deps;

    let root_dir = match persistent_dir {
        Some(dir) => rt.read(dir),
        None => rt.sh.current_dir(),
    };

    let bsdtar = crate::_util::bsdtar_name(rt);
    extract_archive_if_new(rt, &root_dir, file, file_version, bsdtar)
}

/// Extracts the given `.tar.gz` `file` into `persistent_dir` (or into
/// [`std::env::current_dir()`], if no persistent dir is available).
///
/// Unlike `.tar.bz2`, `.tar.gz` is handled natively by every platform's `tar`,
/// so this helper has no install-package dependency to track. The caller
/// resolves the persistent dir itself and passes it (already read) as
/// `persistent_dir` — no `Deps` struct needed.
///
/// To avoid redundant extracts between pipeline runs, callers must provide a
/// `file_version` string that identifies the current file. If the previous
/// run already extracted an archive with the given `file_version`, this
/// function will return nearly instantaneously.
pub fn extract_tar_gz_if_new(
    rt: &mut RustRuntimeServices<'_>,
    persistent_dir: Option<&Path>,
    file: &Path,
    file_version: &str,
) -> anyhow::Result<PathBuf> {
    let root_dir = match persistent_dir {
        Some(dir) => dir.to_path_buf(),
        None => rt.sh.current_dir(),
    };

    extract_archive_if_new(rt, &root_dir, file, file_version, "tar")
}

#[derive(Clone)]
#[non_exhaustive]
pub struct ExtractTarBz2Deps<C = VarNotClaimed> {
    persistent_dir: Option<ReadVar<PathBuf, C>>,
    bzip2_installed: ReadVar<SideEffect, C>,
}

impl ClaimVar for ExtractTarBz2Deps {
    type Claimed = ExtractTarBz2Deps<VarClaimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Self::Claimed {
        let Self {
            persistent_dir,
            bzip2_installed,
        } = self;
        ExtractTarBz2Deps {
            persistent_dir: persistent_dir.claim(ctx),
            bzip2_installed: bzip2_installed.claim(ctx),
        }
    }
}

#[track_caller]
pub fn extract_tar_bz2_if_new_deps(ctx: &mut NodeCtx<'_>) -> ExtractTarBz2Deps {
    ExtractTarBz2Deps {
        persistent_dir: ctx.persistent_dir(),
        bzip2_installed: ctx.reqv(|v| crate::install_dist_pkg::Request::Install {
            package_names: vec!["bzip2".into()],
            done: v,
        }),
    }
}

/// Extracts the given `file` into `persistent_dir` (or into
/// [`std::env::current_dir()`], if no persistent dir is available).
///
/// To avoid redundant extractions between pipeline runs, callers must provide a
/// `file_version` string that identifies the current file. If the previous run
/// already extracted an archive with the given `file_version`, this function will
/// return nearly instantaneously.
pub fn extract_tar_bz2_if_new(
    rt: &mut RustRuntimeServices<'_>,
    deps: ExtractTarBz2Deps<VarClaimed>,
    file: &Path,
    file_version: &str,
) -> anyhow::Result<PathBuf> {
    let ExtractTarBz2Deps {
        persistent_dir,
        bzip2_installed: _,
    } = deps;

    let root_dir = match persistent_dir {
        Some(dir) => rt.read(dir),
        None => rt.sh.current_dir(),
    };

    extract_archive_if_new(rt, &root_dir, file, file_version, "tar")
}

fn extract_archive_if_new(
    rt: &mut RustRuntimeServices<'_>,
    root_dir: &Path,
    file: &Path,
    file_version: &str,
    tar: &str,
) -> anyhow::Result<PathBuf> {
    let current_dir = rt.sh.current_dir();
    let file = current_dir.join(file).absolute()?;
    let root_dir = current_dir.join(root_dir).absolute()?;
    extract_if_new(&root_dir, &file, file_version, |extract_dir| {
        let _dir = rt.sh.push_dir(extract_dir);
        flowey::shell_cmd!(rt, "{tar} -xf {file}").run()?;
        Ok(())
    })
}

fn extract_if_new(
    root_dir: &Path,
    file: &Path,
    file_version: &str,
    extract: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<PathBuf> {
    let filename = file
        .file_name()
        .with_context(|| format!("archive path has no filename: {}", file.display()))?;
    let extract_dir = root_dir.join(FLOWEY_EXTRACT_DIR).join(filename);
    let pkg_info_dir = root_dir.join(FLOWEY_INFO_DIR);
    let pkg_info_file = pkg_info_dir.join(filename);

    let cached_version = match fs_err::read_to_string(&pkg_info_file) {
        Ok(info) => Some(info),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).context("failed to read archive extraction version"),
    };
    let extracted = match fs_err::metadata(&extract_dir) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir(),
                "archive extraction path is not a directory: {}",
                extract_dir.display()
            );
            true
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err).context("failed to inspect archive extraction directory"),
    };

    if extracted && cached_version.as_deref() == Some(file_version) {
        log::info!("already extracted!");
        return Ok(extract_dir);
    }

    // Invalidate the old marker before replacing files, so a failed extraction
    // cannot leave a partial directory that looks like a cache hit.
    if cached_version.is_some() {
        fs_err::remove_file(&pkg_info_file)?;
    }
    if extracted {
        fs_err::remove_dir_all(&extract_dir)?;
    }
    fs_err::create_dir_all(&extract_dir)?;
    extract(&extract_dir)
        .with_context(|| format!("failed to extract archive {}", file.display()))?;
    fs_err::create_dir_all(&pkg_info_dir)?;
    fs_err::write(pkg_info_file, file_version)?;

    Ok(extract_dir)
}
