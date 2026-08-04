// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::FmtPass;
use crate::fs_helpers::git_diffed;
use crate::shell::XtaskShell;
use crate::tasks::fmt::FmtCtx;
use anyhow::Context;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// Windows caps a process's command line at 32767 characters. Batches are
/// normally kept far below this to spread work across threads; this is only a
/// backstop for unusually long paths.
const MAX_FILE_ARG_BYTES: usize = 24 * 1024;

/// Spawning `rustfmt` costs more than formatting a handful of files, so batches
/// below this size are not worth the extra parallelism. Measured on this repo,
/// one process per file is ~3x slower than batching.
const MIN_BATCH_FILES: usize = 8;

/// A set of files to format together under a common edition.
struct Group {
    edition: Option<String>,
    files: Vec<PathBuf>,
}

/// Metadata about one workspace package.
struct Package {
    /// The directory containing the package's `Cargo.toml`.
    dir: PathBuf,
    edition: String,
    /// The crate root of each of the package's targets.
    roots: Vec<PathBuf>,
}

pub struct Rustfmt;

impl FmtPass for Rustfmt {
    fn run(self, ctx: FmtCtx) -> anyhow::Result<()> {
        let FmtCtx {
            ctx,
            fix,
            only_diffed,
        } = ctx;
        let sh = XtaskShell::new()?;
        let rust_toolchain = sh.var("RUST_TOOLCHAIN").map(|s| format!("+{s}")).ok();
        let fmt_check = (!fix).then_some("--check");

        let packages = workspace_packages(&sh)?;

        let groups = if only_diffed {
            let mut files = git_diffed(ctx.in_git_hook)?;
            files.retain(|f| f.extension().unwrap_or_default() == "rs");
            group_diffed(&packages, files)
        } else {
            // Deliberately avoid `cargo fmt`: it spawns a single `rustfmt` with
            // every crate root in the workspace on the command line, which
            // overflows the Windows command line length limit and leaves all
            // but one core idle. Replicate what it does, but in batches.
            group_roots(&packages)
        };

        if run_rustfmt(rust_toolchain.as_deref(), fmt_check, &groups)? {
            anyhow::bail!("found formatting issues");
        }

        Ok(())
    }
}

/// Group every package's crate roots by edition. `rustfmt` walks `mod`
/// declarations itself, so these roots transitively cover every file that
/// `cargo fmt` would format.
fn group_roots(packages: &[Package]) -> Vec<Group> {
    let mut by_edition: BTreeMap<&str, BTreeSet<&Path>> = BTreeMap::new();
    for package in packages {
        by_edition
            .entry(&package.edition)
            .or_default()
            .extend(package.roots.iter().map(PathBuf::as_path));
    }

    by_edition
        .into_iter()
        .map(|(edition, files)| Group {
            edition: Some(edition.to_owned()),
            files: files.into_iter().map(Path::to_path_buf).collect(),
        })
        .collect()
}

/// Group diffed files by the edition of the innermost package containing them,
/// so that they are parsed the same way as in a full run.
fn group_diffed(packages: &[Package], files: Vec<PathBuf>) -> Vec<Group> {
    let mut by_edition: BTreeMap<Option<&str>, Vec<PathBuf>> = BTreeMap::new();
    for file in files {
        let edition = packages
            .iter()
            .filter(|p| file.starts_with(&p.dir))
            .max_by_key(|p| p.dir.components().count())
            .map(|p| p.edition.as_str());

        by_edition.entry(edition).or_default().push(file);
    }

    by_edition
        .into_iter()
        .map(|(edition, files)| Group {
            edition: edition.map(str::to_owned),
            files,
        })
        .collect()
}

/// Run `rustfmt` over each group, splitting the groups into batches that run in
/// parallel.
///
/// Returns whether any invocation reported formatting issues.
fn run_rustfmt(
    rust_toolchain: Option<&str>,
    fmt_check: Option<&str>,
    groups: &[Group],
) -> anyhow::Result<bool> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let total_files: usize = groups.iter().map(|g| g.files.len()).sum();
    let max_files = batch_size(total_files, threads);

    let batches = groups
        .iter()
        .flat_map(|g| {
            batch_files(&g.files, MAX_FILE_ARG_BYTES, max_files)
                .into_iter()
                .map(move |files| (g.edition.as_deref(), files))
        })
        .collect::<Vec<_>>();

    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);

    std::thread::scope(|scope| -> anyhow::Result<()> {
        let handles = (0..threads.min(batches.len()))
            .map(|_| {
                scope.spawn(|| -> anyhow::Result<()> {
                    // `xshell::Shell` isn't `Sync`, so give each thread its own.
                    let sh = XtaskShell::new()?;
                    while let Some((edition, files)) =
                        batches.get(next.fetch_add(1, Ordering::Relaxed))
                    {
                        let res = sh
                            .cmd("rustfmt")
                            .args(rust_toolchain)
                            .args(fmt_check)
                            .args(edition.map(|e| format!("--edition={e}")))
                            .args(*files)
                            .quiet()
                            .run();

                        if res.is_err() {
                            failed.store(true, Ordering::Relaxed);
                        }
                    }
                    Ok(())
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap()?;
        }
        Ok(())
    })?;

    Ok(failed.load(Ordering::Relaxed))
}

/// Choose how many files to put in each batch: enough batches to keep every
/// thread busy through the end of the run, but not so few files per batch that
/// process startup dominates.
fn batch_size(total_files: usize, threads: usize) -> usize {
    total_files.div_ceil(threads * 4).max(MIN_BATCH_FILES)
}

/// Split `files` into batches that fit in a command line and are small enough
/// to spread across the available threads.
fn batch_files(files: &[PathBuf], max_bytes: usize, max_files: usize) -> Vec<&[PathBuf]> {
    let mut batches = Vec::new();
    let mut start = 0;
    let mut len = 0;

    for (i, file) in files.iter().enumerate() {
        // +1 for the argument separator
        let file_len = file.as_os_str().len() + 1;
        if i > start && (len + file_len > max_bytes || i - start >= max_files) {
            batches.push(&files[start..i]);
            start = i;
            len = 0;
        }
        len += file_len;
    }

    if start < files.len() {
        batches.push(&files[start..]);
    }

    batches
}

/// Collect the edition, directory, and crate roots of every workspace package.
fn workspace_packages(sh: &XtaskShell) -> anyhow::Result<Vec<Package>> {
    #[derive(serde::Deserialize)]
    struct Metadata {
        packages: Vec<MetadataPackage>,
    }

    #[derive(serde::Deserialize)]
    struct MetadataPackage {
        edition: String,
        manifest_path: PathBuf,
        targets: Vec<Target>,
    }

    #[derive(serde::Deserialize)]
    struct Target {
        src_path: PathBuf,
    }

    let output = sh
        .cmd("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .quiet()
        .output()
        .context("failed to run cargo metadata")?;

    let metadata: Metadata =
        serde_json::from_slice(&output.stdout).context("failed to parse cargo metadata")?;

    let cwd = std::env::current_dir()?;
    // Shorten the paths as much as possible, since the command line length is
    // the constraint being worked around here.
    let shorten = |path: &Path| path.strip_prefix(&cwd).unwrap_or(path).to_path_buf();

    let mut packages = Vec::new();
    for package in metadata.packages {
        let dir = package
            .manifest_path
            .parent()
            .context("manifest path has no parent")?;

        let mut roots: Vec<PathBuf> = package
            .targets
            .iter()
            .map(|t| shorten(&t.src_path))
            .collect();
        roots.sort();
        roots.dedup();

        packages.push(Package {
            dir: shorten(dir),
            edition: package.edition,
            roots,
        });
    }

    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 24 * 1024;

    fn test_files() -> Vec<PathBuf> {
        (0..10_000)
            .map(|i| PathBuf::from(format!("some/moderately/long/path/to/crate{i}/src/lib.rs")))
            .collect()
    }

    #[test]
    fn batches_stay_under_limit() {
        let files = test_files();
        let batches = batch_files(&files, LIMIT, usize::MAX);
        assert!(batches.len() > 1);
        assert_eq!(batches.iter().map(|b| b.len()).sum::<usize>(), files.len());

        for batch in batches {
            let len: usize = batch.iter().map(|f| f.as_os_str().len() + 1).sum();
            assert!(len <= LIMIT, "batch too long: {len}");
        }
    }

    #[test]
    fn batches_stay_under_file_count() {
        let files = test_files();
        let batches = batch_files(&files, usize::MAX, 64);
        assert_eq!(batches.len(), files.len().div_ceil(64));
        assert!(batches.iter().all(|b| b.len() <= 64));
    }

    #[test]
    fn single_oversized_file_still_batched() {
        let files = vec![PathBuf::from("a".repeat(LIMIT * 2))];
        assert_eq!(batch_files(&files, LIMIT, usize::MAX).len(), 1);
    }

    #[test]
    fn unlimited_produces_one_batch() {
        let files = test_files();
        assert_eq!(batch_files(&files, usize::MAX, usize::MAX).len(), 1);
    }

    #[test]
    fn empty_input_produces_no_batches() {
        assert!(batch_files(&[], LIMIT, usize::MAX).is_empty());
    }

    #[test]
    fn small_runs_use_a_single_batch() {
        assert_eq!(batch_size(8, 8), MIN_BATCH_FILES);
        assert_eq!(batch_size(1, 8), MIN_BATCH_FILES);
    }

    #[test]
    fn large_runs_spread_across_threads() {
        assert_eq!(batch_size(483, 8), 16);
        assert!(483_usize.div_ceil(batch_size(483, 8)) >= 8);
    }

    #[test]
    fn diffed_files_use_innermost_package_edition() {
        let packages = vec![
            Package {
                dir: PathBuf::from(""),
                edition: "2021".to_string(),
                roots: Vec::new(),
            },
            Package {
                dir: PathBuf::from("vm/devices/net"),
                edition: "2024".to_string(),
                roots: Vec::new(),
            },
        ];

        let groups = group_diffed(
            &packages,
            vec![
                PathBuf::from("xtask/src/main.rs"),
                PathBuf::from("vm/devices/net/netvsp/src/lib.rs"),
            ],
        );

        let editions: Vec<_> = groups
            .iter()
            .map(|g| (g.edition.as_deref(), g.files.clone()))
            .collect();
        assert_eq!(
            editions,
            vec![
                (Some("2021"), vec![PathBuf::from("xtask/src/main.rs")]),
                (
                    Some("2024"),
                    vec![PathBuf::from("vm/devices/net/netvsp/src/lib.rs")]
                ),
            ]
        );
    }
}
