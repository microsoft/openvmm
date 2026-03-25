use crate::fs_helpers::git_diffed;
use crate::tasks::fmt::FmtCtx;
use crate::tasks::fmt::FmtPass;
use std::collections::BTreeSet;
use std::fmt::Display;
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use toml_edit::DocumentMut;

pub struct LintCtx {
    /// When true we are linting a subset of repo files, so some lints may want
    /// to skip checks that require whole-repo analysis.
    only_diffed: bool,
}

pub trait Lint {
    fn new(ctx: &LintCtx) -> Self
    where
        Self: Sized;
    fn enter_workspace(&mut self, content: &Lintable<DocumentMut>);
    fn enter_crate(&mut self, content: &Lintable<DocumentMut>);
    fn visit_file(&mut self, content: &mut Lintable<String>);
    fn exit_crate(&mut self, content: &mut Lintable<DocumentMut>);
    fn exit_workspace(&mut self, content: &mut Lintable<DocumentMut>);

    fn visit_nonrust_file(&mut self, extension: &str, content: &mut Lintable<String>) {
        let _ = (extension, content);
    }
}

pub struct Lintable<T> {
    content: T,
    fix: bool,
    path: PathBuf,
    modified: bool,
    failed: bool,
}

impl<T> Deref for Lintable<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.content
    }
}

impl Lintable<String> {
    /// Returns `None` for binary (non-UTF-8) files.
    fn from_file(path: &Path, ctx: &FmtCtx) -> anyhow::Result<Option<Self>> {
        let bytes = fs_err::read(path)?;
        let content = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        Ok(Some(Self {
            content,
            fix: ctx.fix,
            path: path.to_owned(),
            modified: false,
            failed: false,
        }))
    }
}

impl Lintable<DocumentMut> {
    fn from_file(path: &Path, ctx: &FmtCtx) -> anyhow::Result<Self> {
        Ok(Self {
            content: fs_err::read_to_string(path)?.parse::<DocumentMut>()?,
            fix: ctx.fix,
            path: path.to_owned(),
            modified: false,
            failed: false,
        })
    }
}

impl<T> Lintable<T> {
    pub fn fix(&mut self, description: &str, op: impl FnOnce(&mut T)) {
        if self.fix {
            op(&mut self.content);
            self.modified = true;
        } else {
            self.failed = true;
        }
    }

    pub fn unfixable(&mut self, description: &str) {
        self.failed = true;
    }

    fn finalize(self) -> anyhow::Result<bool>
    where
        T: Display,
    {
        if self.modified {
            fs_err::write(&self.path, self.content.to_string())?;
        }
        if self.failed {
            log::error!("lint failure in {}", self.path.display());
        }
        Ok(self.failed)
    }
}

pub struct Lints;

impl FmtPass for Lints {
    fn run(self, ctx: FmtCtx) -> anyhow::Result<()> {
        let lint_ctx = LintCtx {
            only_diffed: ctx.only_diffed,
        };

        let mut lints: Vec<Box<dyn Lint>> = vec![
            // add lints here
        ];

        // Determine which files are diffed, if applicable.
        let diffed_files: Option<Vec<PathBuf>> = if ctx.only_diffed {
            Some(git_diffed(ctx.ctx.in_git_hook)?)
        } else {
            None
        };

        // Load the workspace root manifest.
        let workspace_manifest_path = ctx.ctx.root.join("Cargo.toml");
        let mut workspace_manifest =
            Lintable::<DocumentMut>::from_file(&workspace_manifest_path, &ctx)?;

        for lint in lints.iter_mut() {
            lint.enter_workspace(&workspace_manifest);
        }

        // Discover crate directories by walking for Cargo.toml files.
        let mut crate_dirs: BTreeSet<PathBuf> = BTreeSet::new();
        // Collect all non-crate files for later processing.
        let mut non_crate_files: Vec<PathBuf> = Vec::new();
        for entry in ignore::Walk::new(&ctx.ctx.root) {
            let entry = entry?;
            if entry.file_name() == "Cargo.toml" {
                let path = entry.into_path();
                if path == workspace_manifest.path {
                    continue;
                }
                crate_dirs.insert(path.parent().unwrap().to_owned());
            } else if entry.file_type().is_some_and(|ft| ft.is_file())
                && entry.path().extension().and_then(|e| e.to_str()) != Some("rs")
            {
                non_crate_files.push(entry.into_path());
            }
        }

        // Filter non-crate files: keep only files not inside any crate dir
        // and not the root Cargo.toml.
        non_crate_files.retain(|f| {
            f != &workspace_manifest.path
                && !crate_dirs.iter().any(|crate_dir| f.starts_with(crate_dir))
        });

        // If only_diffed, filter both crate dirs and non-crate files.
        if let Some(ref diffed) = diffed_files {
            crate_dirs.retain(|crate_dir| diffed.iter().any(|f| f.starts_with(crate_dir)));
            non_crate_files.retain(|f| diffed.contains(f));
        }

        let mut any_failed = false;

        for crate_dir in &crate_dirs {
            let manifest_path = crate_dir.join("Cargo.toml");
            let mut crate_manifest = Lintable::<DocumentMut>::from_file(&manifest_path, &ctx)?;

            for lint in lints.iter_mut() {
                lint.enter_crate(&crate_manifest);
            }

            // Collect nested crate dirs within this crate to avoid
            // processing files that belong to a child crate.
            let nested_crate_dirs: Vec<&PathBuf> = crate_dirs
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
                let Some(mut file) = Lintable::<String>::from_file(&path, &ctx)? else {
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
        for path in &non_crate_files {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let Some(mut file) = Lintable::<String>::from_file(path, &ctx)? else {
                // Skip binary files
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

        if any_failed {
            anyhow::bail!("one or more lint checks failed");
        }

        Ok(())
    }
}
