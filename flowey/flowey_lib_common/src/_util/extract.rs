// Copyright (C) Microsoft Corporation. All rights reserved.

use flowey::node::prelude::*;

const FLOWEY_INFO_DIR: &str = ".flowey_info";
const FLOWEY_EXTRACT_DIR: &str = "extracted";

#[derive(Clone)]
#[non_exhaustive]
pub struct ExtractArchiveDeps<C = VarNotClaimed> {
    persistent_dir: Option<ReadVar<PathBuf, C>>,
    bsdtar_installed: ReadVar<SideEffect, C>,
}

impl ClaimVar for ExtractArchiveDeps {
    type Claimed = ExtractArchiveDeps<VarClaimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Self::Claimed {
        let Self {
            persistent_dir,
            bsdtar_installed,
        } = self;
        ExtractArchiveDeps {
            persistent_dir: persistent_dir.claim(ctx),
            bsdtar_installed: bsdtar_installed.claim(ctx),
        }
    }
}

#[track_caller]
pub fn extract_archive_if_new_deps(ctx: &mut NodeCtx<'_>) -> ExtractArchiveDeps {
    ExtractArchiveDeps {
        persistent_dir: ctx.persistent_dir(),
        bsdtar_installed: ctx.reqv(|v| crate::install_apt_pkg::Request::Install {
            package_names: vec!["libarchive-tools".into()],
            done: v,
        }),
    }
}

/// Extracts the given `file` into `persistent_dir` (or into
/// [`std::env::current_dir()`], if no persistent dir is available).
///
/// The file must be of a type supported by bsdtar (libarchive).
///
/// To avoid redundant extracts between pipeline runs, callers must provide a
/// `file_version` string that identifies the current file. If the previous run
/// already extracted a file with the given `file_version`, this function will
/// return nearly instantaneously.
pub fn extract_archive_if_new(
    rt: &mut RustRuntimeServices<'_>,
    deps: ExtractArchiveDeps<VarClaimed>,
    file: &Path,
    file_version: &str,
) -> anyhow::Result<PathBuf> {
    let ExtractArchiveDeps {
        persistent_dir,
        bsdtar_installed: _,
    } = deps;

    let sh = xshell::Shell::new()?;

    let root_dir = match persistent_dir {
        Some(dir) => rt.read(dir),
        None => sh.current_dir(),
    };

    let filename = file.file_name().expect("archive file was not a file");
    let pkg_info_dir = root_dir.join(FLOWEY_INFO_DIR);
    fs_err::create_dir_all(&pkg_info_dir)?;
    let pkg_info_file = pkg_info_dir.join(filename);

    let mut already_extracted = false;
    let extract_dir = root_dir.join(FLOWEY_EXTRACT_DIR).join(filename);
    if extract_dir.is_dir() {
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
        }
    } else {
        // Ensure there's no stale info file, in case the user removed the
        // directory manually.
        match fs_err::remove_file(&pkg_info_file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }

    if !already_extracted {
        fs_err::create_dir_all(&extract_dir)?;
        let bsdtar = crate::_util::bsdtar_name(rt);
        xshell::cmd!(sh, "{bsdtar} -xf {file} -C {extract_dir}").run()?;
        fs_err::write(pkg_info_file, file_version)?;
    } else {
        log::info!("already extracted!");
    }

    Ok(extract_dir)
}
