// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;

const FLOWEY_EXTRACT_DIR: &str = "extracted";

/// Bump this whenever the extraction logic changes in a way that could affect
/// the extracted output.
const EXTRACT_MEMO_VERSION: u32 = 1;

#[derive(Clone)]
#[non_exhaustive]
pub struct ExtractZipDeps<C = VarNotClaimed> {
    memo_store: Option<ReadVar<MemoStore, C>>,
    bsdtar_installed: ReadVar<SideEffect, C>,
}

impl ClaimVar for ExtractZipDeps {
    type Claimed = ExtractZipDeps<VarClaimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Self::Claimed {
        let Self {
            memo_store,
            bsdtar_installed,
        } = self;
        ExtractZipDeps {
            memo_store: memo_store.claim(ctx),
            bsdtar_installed: bsdtar_installed.claim(ctx),
        }
    }
}

#[track_caller]
pub fn extract_zip_if_new_deps(ctx: &mut NodeCtx<'_>) -> ExtractZipDeps {
    let platform = ctx.platform();
    ExtractZipDeps {
        memo_store: ctx.memo_store(),
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

/// Extract `file` into flowey's memoization store, returning the directory it
/// was extracted into.
///
/// To avoid redundant extractions between runs, callers must provide a
/// `file_version` string that identifies the current file. If a previous run
/// already extracted an identical archive with the given `file_version`, this
/// returns nearly instantaneously.
pub fn extract_zip_if_new(
    rt: &mut RustRuntimeServices<'_>,
    deps: ExtractZipDeps<VarClaimed>,
    file: &Path,
    file_version: &str,
) -> anyhow::Result<PathBuf> {
    let ExtractZipDeps {
        memo_store,
        bsdtar_installed: _,
    } = deps;

    let memo_store = memo_store.map(|v| rt.read(v));
    let bsdtar = crate::_util::bsdtar_name(rt);

    extract_memoized(
        rt,
        memo_store.as_ref(),
        file,
        file_version,
        "bsdtar",
        |rt| {
            flowey::shell_cmd!(rt, "{bsdtar} -xf {file}").run()?;
            Ok(())
        },
    )
}

/// Extract the given `.tar.gz` `file` into flowey's memoization store,
/// returning the directory it was extracted into.
///
/// Unlike `.tar.bz2`, `.tar.gz` is handled natively by every platform's `tar`,
/// so this helper has no install-package dependency to track. The caller
/// resolves the store itself (via [`NodeCtx::memo_store`]) and passes it
/// (already read) as `memo_store` — no `Deps` struct needed.
///
/// See [`extract_zip_if_new`] for the meaning of `file_version`.
pub fn extract_tar_gz_if_new(
    rt: &mut RustRuntimeServices<'_>,
    memo_store: Option<&MemoStore>,
    file: &Path,
    file_version: &str,
) -> anyhow::Result<PathBuf> {
    extract_memoized(rt, memo_store, file, file_version, "tar-gz", |rt| {
        // windows builds past Windows 10 build 17063 come with tar installed,
        // and `tar -xf` auto-detects gzip compression on all platforms
        flowey::shell_cmd!(rt, "tar -xf {file}").run()?;
        Ok(())
    })
}

#[derive(Clone)]
#[non_exhaustive]
pub struct ExtractTarBz2Deps<C = VarNotClaimed> {
    memo_store: Option<ReadVar<MemoStore, C>>,
    bzip2_installed: ReadVar<SideEffect, C>,
}

impl ClaimVar for ExtractTarBz2Deps {
    type Claimed = ExtractTarBz2Deps<VarClaimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Self::Claimed {
        let Self {
            memo_store,
            bzip2_installed,
        } = self;
        ExtractTarBz2Deps {
            memo_store: memo_store.claim(ctx),
            bzip2_installed: bzip2_installed.claim(ctx),
        }
    }
}

#[track_caller]
pub fn extract_tar_bz2_if_new_deps(ctx: &mut NodeCtx<'_>) -> ExtractTarBz2Deps {
    ExtractTarBz2Deps {
        memo_store: ctx.memo_store(),
        bzip2_installed: ctx.reqv(|v| crate::install_dist_pkg::Request::Install {
            package_names: vec!["bzip2".into()],
            done: v,
        }),
    }
}

/// Extract the given `.tar.bz2` `file` into flowey's memoization store,
/// returning the directory it was extracted into.
///
/// See [`extract_zip_if_new`] for the meaning of `file_version`.
pub fn extract_tar_bz2_if_new(
    rt: &mut RustRuntimeServices<'_>,
    deps: ExtractTarBz2Deps<VarClaimed>,
    file: &Path,
    file_version: &str,
) -> anyhow::Result<PathBuf> {
    let ExtractTarBz2Deps {
        memo_store,
        bzip2_installed: _,
    } = deps;

    let memo_store = memo_store.map(|v| rt.read(v));

    extract_memoized(
        rt,
        memo_store.as_ref(),
        file,
        file_version,
        "tar-bz2",
        |rt| {
            // windows builds past Windows 10 build 17063 come with tar installed
            flowey::shell_cmd!(rt, "tar -xf {file}").run()?;
            Ok(())
        },
    )
}

/// Shared implementation of the `extract_*_if_new` helpers.
///
/// The archive is keyed by its `(len, mtime)` stamp _and_ the caller-provided
/// `file_version`, so unlike the old "one extraction dir per filename" scheme,
/// multiple versions of the same archive can coexist - switching branches no
/// longer forces a re-extract.
fn extract_memoized(
    rt: &mut RustRuntimeServices<'_>,
    memo_store: Option<&MemoStore>,
    file: &Path,
    file_version: &str,
    kind: &str,
    extract: impl FnOnce(&mut RustRuntimeServices<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<PathBuf> {
    let filename = file
        .file_name()
        .context("archive path did not name a file")?
        .to_string_lossy()
        .into_owned();

    let Some(store) = memo_store else {
        // no persistent storage available (e.g: CI) - extract into the step's
        // working dir.
        let extract_dir = rt.sh.current_dir().join(FLOWEY_EXTRACT_DIR).join(&filename);
        if extract_dir.exists() {
            fs_err::remove_dir_all(&extract_dir)?;
        }
        fs_err::create_dir_all(&extract_dir)?;
        extract_in_dir(rt, &extract_dir, extract)?;
        return Ok(extract_dir);
    };

    let key = MemoKey::new("flowey_lib_common::_util::extract", EXTRACT_MEMO_VERSION)
        .with_str("kind", kind)
        .with_value("filename", &filename)
        .with_str("file_version", file_version)
        .with_path(rt, "archive", file)?;

    let entry =
        store.get_or_insert_with(&key, move |out_dir| extract_in_dir(rt, out_dir, extract))?;

    Ok(entry.dir)
}

/// Run `extract` with the shell's working dir temporarily set to `dir`.
///
/// Restoring the original dir keeps the step's shell state identical whether
/// or not the extraction was memoized away.
fn extract_in_dir(
    rt: &mut RustRuntimeServices<'_>,
    dir: &Path,
    extract: impl FnOnce(&mut RustRuntimeServices<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let orig = rt.sh.current_dir();
    rt.sh.change_dir(dir);
    let res = extract(rt);
    rt.sh.change_dir(orig);
    res
}
