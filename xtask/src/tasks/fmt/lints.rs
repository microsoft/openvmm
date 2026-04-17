// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A harness for running custom text-based lints over repository files.

mod cfg_target_arch;
mod copyright;
mod crate_name_nodash;
mod package_info;
mod repr_packed;
mod trailing_newline;
mod unsafe_code_comment;
mod unused_deps;
mod workspaced;

use crate::fs_helpers::git_diffed;
use crate::tasks::fmt::FmtCtx;
use crate::tasks::fmt::FmtPass;
use std::fmt::Display;
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use toml_edit::DocumentMut;

/// Context passed to each lint, containing configuration options.
pub struct LintCtx {
    /// When true we are linting a subset of repo files, so some lints may want
    /// to skip checks that require whole-repo analysis.
    only_diffed: bool,
}

/// A trait representing a single lint check.
pub trait Lint {
    /// Create a new instance of this lint for a workspace.
    fn new(ctx: &LintCtx) -> Self
    where
        Self: Sized;

    /// Begin processing a workspace, given the parsed Cargo.toml of the workspace root.
    fn enter_workspace(&mut self, content: &Lintable<DocumentMut>);

    /// Begin processing a crate, given the parsed Cargo.toml of the crate root.
    fn enter_crate(&mut self, content: &Lintable<DocumentMut>);

    /// Process a Rust source file in the current crate.
    fn visit_file(&mut self, content: &mut Lintable<String>);

    /// Finish processing a crate, given the parsed Cargo.toml of the crate root.
    fn exit_crate(&mut self, content: &mut Lintable<DocumentMut>);

    /// Finish processing a workspace, given the parsed Cargo.toml of the workspace root.
    fn exit_workspace(&mut self, content: &mut Lintable<DocumentMut>);

    /// Process a non-Rust file in the current crate or workspace.
    ///
    /// For files within the directory of a crate this is called during crate processing.
    /// For files outside of any crate this is called during workspace processing after
    /// all crates have been processed.
    fn visit_nonrust_file(&mut self, extension: &str, content: &mut Lintable<String>) {
        let _ = (extension, content);
    }
}

/// A wrapper around file content for linting.
///
/// Most lints will want to use the `Deref` impl to access the content directly,
/// but this also provides utilities for reporting errors and making fixes.
pub struct Lintable<T> {
    content: T,
    raw: Option<String>,
    fix: bool,
    path: PathBuf,
    workspace_dir: PathBuf,
    modified: bool,
    // This doesn't really need to be atomic, but it lets `unfixable` only take
    // `&self` which is more convenient.
    failed: AtomicBool,
}

impl<T> Deref for Lintable<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.content
    }
}

impl Lintable<String> {
    /// Read a text file into a `Lintable<String>`.
    ///
    /// Returns `None` for binary (non-UTF-8) files.
    fn from_file(path: &Path, ctx: &FmtCtx, workspace_dir: &Path) -> anyhow::Result<Option<Self>> {
        let bytes = fs_err::read(path)?;
        let content = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        Ok(Some(Self {
            content,
            raw: None,
            fix: ctx.fix,
            path: path.strip_prefix(workspace_dir).unwrap().to_owned(),
            workspace_dir: workspace_dir.to_owned(),
            modified: false,
            failed: AtomicBool::new(false),
        }))
    }
}

impl Lintable<DocumentMut> {
    /// Read a Cargo.toml file into a `Lintable<DocumentMut>`.
    ///
    /// This can be from a crate or a workspace.
    fn from_file(path: &Path, ctx: &FmtCtx, workspace_dir: &Path) -> anyhow::Result<Self> {
        let raw = fs_err::read_to_string(path)?;
        Ok(Self {
            content: raw.parse()?,
            raw: Some(raw),
            fix: ctx.fix,
            path: path.strip_prefix(workspace_dir).unwrap().to_owned(),
            workspace_dir: workspace_dir.to_owned(),
            modified: false,
            failed: AtomicBool::new(false),
        })
    }
}

impl<T> Lintable<T> {
    /// Get the path of this file relative to the workspace root, for use in error messages.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the original raw file content as a string, for lints that need to do their own parsing.
    ///
    /// If the file content is already a string this will be None.
    /// This field is not modified when fixes are made.
    pub fn raw(&self) -> Option<&str> {
        self.raw.as_deref()
    }

    /// If fix is enabled, apply the given fix operation to the content and mark it modified.
    /// If fix is not enabled, report an error with the given description.
    pub fn fix(&mut self, description: &str, op: impl FnOnce(&mut T)) {
        if self.fix {
            op(&mut self.content);
            self.modified = true;
        } else {
            log::error!("{}: {}", self.path.display(), description);
            self.failed
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Report an error with the given description that cannot be automatically fixed.
    pub fn unfixable(&self, description: &str) {
        log::error!("{}: {}", self.path.display(), description);
        self.failed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// If modified, write the content back to the file. Return whether any errors were reported.
    fn finalize(self) -> anyhow::Result<bool>
    where
        T: Display,
    {
        if self.modified {
            let full_path = self.workspace_dir.join(&self.path);
            fs_err::write(full_path, self.content.to_string())?;
        }
        Ok(self.failed.into_inner())
    }
}

pub struct Lints;

impl FmtPass for Lints {
    fn run(self, ctx: FmtCtx) -> anyhow::Result<()> {
        // Walk tree once to discover all Cargo.toml files and non-Rust,
        // non-manifest files.
        let mut workspace_dirs = Vec::new();
        let mut all_crate_dirs = Vec::new();
        let mut all_other_files = Vec::new();
        for entry in ignore::Walk::new(&ctx.ctx.root) {
            let entry = entry?;
            if entry.file_name() == "Cargo.toml" {
                // Identify workspace roots (Cargo.toml files with a [workspace] key).
                let raw = fs_err::read_to_string(entry.path())?;
                let doc: DocumentMut = raw.parse()?;
                if doc.contains_key("workspace") {
                    workspace_dirs.push(entry.path().parent().unwrap().to_owned());
                } else {
                    // Build the set of all crate directories (every Cargo.toml parent
                    // that is not itself a workspace root).
                    all_crate_dirs.push(entry.path().parent().unwrap().to_owned());
                }
            } else if entry.file_type().is_some_and(|ft| ft.is_file())
                && entry.path().extension().and_then(|e| e.to_str()) != Some("rs")
            {
                all_other_files.push(entry.into_path());
            }
        }

        let mut any_failed = false;

        // Run a fresh set of lints over each workspace.
        for workspace_dir in &workspace_dirs {
            // Nested workspace dirs that are children of this workspace.
            let nested_workspace_dirs: Vec<_> = workspace_dirs
                .iter()
                .filter(|other| *other != workspace_dir && other.starts_with(workspace_dir))
                .collect();

            // Crate dirs belonging to this workspace: under workspace_dir
            // but not under any deeper nested workspace.
            let mut crate_dirs: Vec<_> = all_crate_dirs
                .iter()
                .filter(|crate_dir| {
                    crate_dir.starts_with(workspace_dir)
                        && !nested_workspace_dirs
                            .iter()
                            .any(|nested| crate_dir.starts_with(*nested))
                })
                .collect();

            // Non-crate files belonging to this workspace.
            let mut non_crate_files: Vec<_> = all_other_files
                .iter()
                .filter(|f| {
                    f.starts_with(workspace_dir)
                        && !nested_workspace_dirs
                            .iter()
                            .any(|nested| f.starts_with(*nested))
                        && !crate_dirs.iter().any(|crate_dir| f.starts_with(crate_dir))
                })
                .collect();

            // If only_diffed, filter crate dirs and non-crate files.
            if ctx.only_diffed {
                let diffed = git_diffed(ctx.ctx.in_git_hook)?;
                // git diff outputs paths relative to the repo root, so strip
                // the root from our other full paths before checking for a match
                crate_dirs.retain(|crate_dir| {
                    let crate_dir = crate_dir.strip_prefix(&ctx.ctx.root).unwrap();
                    diffed.iter().any(|f| f.starts_with(crate_dir))
                });
                non_crate_files.retain(|f| {
                    let f = f.strip_prefix(&ctx.ctx.root).unwrap().to_owned();
                    diffed.contains(&f)
                });
            }

            any_failed |= lint_workspace(workspace_dir, &crate_dirs, &non_crate_files, &ctx)?;
        }

        if any_failed {
            anyhow::bail!("one or more lint checks failed");
        }

        Ok(())
    }
}

/// Run a fresh set of lints over a single workspace and its member crates.
fn lint_workspace(
    workspace_dir: &Path,
    crate_dirs: &[&PathBuf],
    non_crate_files: &[&PathBuf],
    ctx: &FmtCtx,
) -> anyhow::Result<bool> {
    let lint_ctx = LintCtx {
        only_diffed: ctx.only_diffed,
    };

    let mut lints: Vec<Box<dyn Lint>> = vec![
        Box::new(cfg_target_arch::CfgTargetArch::new(&lint_ctx)),
        Box::new(copyright::Copyright::new(&lint_ctx)),
        Box::new(crate_name_nodash::CrateNameNoDash::new(&lint_ctx)),
        Box::new(package_info::PackageInfo::new(&lint_ctx)),
        Box::new(repr_packed::ReprPacked::new(&lint_ctx)),
        Box::new(trailing_newline::TrailingNewline::new(&lint_ctx)),
        Box::new(unsafe_code_comment::UnsafeCodeComment::new(&lint_ctx)),
        Box::new(unused_deps::UnusedDeps::new(&lint_ctx)),
        Box::new(workspaced::WorkspacedManifest::new(&lint_ctx)),
    ];

    let workspace_manifest_path = workspace_dir.join("Cargo.toml");
    let mut workspace_manifest =
        Lintable::<DocumentMut>::from_file(&workspace_manifest_path, ctx, workspace_dir)?;

    log::debug!(
        "Linting workspace {} with {} crates and {} non-crate files",
        workspace_dir.display(),
        crate_dirs.len(),
        non_crate_files.len()
    );
    for lint in lints.iter_mut() {
        lint.enter_workspace(&workspace_manifest);
    }

    let mut any_failed = false;

    for crate_dir in crate_dirs {
        let manifest_path = crate_dir.join("Cargo.toml");
        let mut crate_manifest =
            Lintable::<DocumentMut>::from_file(&manifest_path, ctx, workspace_dir)?;

        log::debug!("Linting crate {}", crate_dir.display());
        for lint in lints.iter_mut() {
            lint.enter_crate(&crate_manifest);
        }

        // Collect nested crate dirs within this crate to avoid
        // processing files that belong to a child crate.
        let nested_crate_dirs: Vec<_> = crate_dirs
            .iter()
            .filter(|other| *other != crate_dir && other.starts_with(crate_dir))
            .collect();

        // Walk all files in the crate directory.
        for entry in ignore::Walk::new(crate_dir) {
            let entry = entry?;
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let path = entry.into_path();

            // Skip Cargo.toml—already handled via enter_crate/exit_crate.
            if path == manifest_path {
                continue;
            }

            // Skip files that belong to a nested crate.
            if nested_crate_dirs
                .iter()
                .any(|nested| path.starts_with(nested))
            {
                continue;
            }

            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let Some(mut file) = Lintable::<String>::from_file(&path, ctx, workspace_dir)? else {
                // Skip binary files
                continue;
            };

            for lint in lints.iter_mut() {
                if ext == "rs" {
                    lint.visit_file(&mut file);
                } else {
                    lint.visit_nonrust_file(ext, &mut file);
                }
            }
            any_failed |= file.finalize()?;
        }

        for lint in lints.iter_mut() {
            lint.exit_crate(&mut crate_manifest);
        }
        any_failed |= crate_manifest.finalize()?;
    }

    // Process non-crate files (e.g. scripts, Guide).
    for path in non_crate_files {
        log::debug!("Linting non-crate file {}", path.display());
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let Some(mut file) = Lintable::<String>::from_file(path, ctx, workspace_dir)? else {
            // Skip binary files
            log::debug!("Skipping binary file {}", path.display());
            continue;
        };
        for lint in lints.iter_mut() {
            lint.visit_nonrust_file(ext, &mut file);
        }
        any_failed |= file.finalize()?;
    }

    for lint in lints.iter_mut() {
        lint.exit_workspace(&mut workspace_manifest);
    }
    any_failed |= workspace_manifest.finalize()?;

    Ok(any_failed)
}
