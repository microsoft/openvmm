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
                    FlowPlatformLinuxDistro::Fedora => vec!["bsdtar".into()],
                    FlowPlatformLinuxDistro::Ubuntu => vec!["libarchive-tools".into()],
                    FlowPlatformLinuxDistro::Arch => vec!["libarchive".into()],
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

    let sh = rt.shell()?;

    let root_dir = match persistent_dir {
        Some(dir) => rt.read(dir),
        None => sh.current_dir(),
    };

    let filename = file.file_name().expect("zip file was not a file");
    let extract_dir = root_dir.join(FLOWEY_EXTRACT_DIR).join(filename);
    fs_err::create_dir_all(&extract_dir)?;

    let pkg_info_dir = root_dir.join(FLOWEY_INFO_DIR);
    fs_err::create_dir_all(&pkg_info_dir)?;
    let pkg_info_file = pkg_info_dir.join(filename);

    let mut already_extracted = false;
    if let Ok(info) = fs_err::read_to_string(&pkg_info_file) {
        if info == file_version {
            already_extracted = true;
        }
    }

    if !already_extracted {
        // clear out any old version that was present
        //
        // FUTURE: maybe reconsider this approach, and keep
        // old versions lying around, to make branch
        // switching easier?
        fs_err::remove_dir_all(&extract_dir)?;
        fs_err::create_dir(&extract_dir)?;

        sh.change_dir(&extract_dir);

        let bsdtar = crate::_util::bsdtar_name(rt);
        xshell::cmd!(sh, "{bsdtar} -xf {file}").run()?;
        fs_err::write(pkg_info_file, file_version)?;
    } else {
        log::info!("already extracted!");
    }

    Ok(extract_dir)
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
/// To avoid redundant unzips between pipeline runs, callers must provide a
/// `file_version` string that identifies the current file. If the previous run
/// already unzipped a zip with the given `file_version`, this function will
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

    let sh = xshell::Shell::new()?;

    let root_dir = match persistent_dir {
        Some(dir) => rt.read(dir),
        None => sh.current_dir(),
    };

    let filename = file.file_name().expect("tar.bz2 file was not a file");
    let extract_dir = root_dir.join(FLOWEY_EXTRACT_DIR).join(filename);
    fs_err::create_dir_all(&extract_dir)?;

    let pkg_info_dir = root_dir.join(FLOWEY_INFO_DIR);
    fs_err::create_dir_all(&pkg_info_dir)?;
    let pkg_info_file = pkg_info_dir.join(filename);

    let mut already_extracted = false;
    if let Ok(info) = fs_err::read_to_string(&pkg_info_file) {
        if info == file_version {
            already_extracted = true;
        }
    }

    if !already_extracted {
        sh.change_dir(&extract_dir);

        // clear out any old version that was present
        //
        // FUTURE: maybe reconsider this approach, and keep
        // old versions lying around, to make branch
        // switching easier?
        fs_err::remove_dir_all(&extract_dir)?;
        fs_err::create_dir(&extract_dir)?;

        // windows builds past Windows 10 build 17063 come with tar installed
        xshell::cmd!(sh, "tar -xf {file}").run()?;

        fs_err::write(pkg_info_file, file_version)?;
    } else {
        log::info!("already extracted!");
    }

    Ok(extract_dir)
}
