// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Warns about Rust source files that are not reachable from a Cargo target.

use super::Lint;
use super::LintCtx;
use super::Lintable;
use std::path::Path;
use std::path::PathBuf;
use toml_edit::DocumentMut;

const SUPPRESS: &str = "xtask-fmt allow-orphaned-rust-file";

struct RustFile {
    path: PathBuf,
    content: String,
}

pub struct OrphanedRustFiles {
    crate_dir: PathBuf,
    manifest: String,
    files: Vec<RustFile>,
}

impl Lint for OrphanedRustFiles {
    fn new(_ctx: &LintCtx) -> Self {
        Self {
            crate_dir: PathBuf::new(),
            manifest: String::new(),
            files: Vec::new(),
        }
    }

    fn enter_workspace(&mut self, _content: &Lintable<DocumentMut>) {}

    fn enter_crate(&mut self, content: &Lintable<DocumentMut>) {
        self.crate_dir = content.path().parent().unwrap_or(Path::new("")).to_owned();
        self.manifest = content.raw().unwrap_or_default().to_owned();
        self.files.clear();
    }

    fn visit_file(&mut self, content: &mut Lintable<String>) {
        self.files.push(RustFile {
            path: content.path().to_owned(),
            content: content.to_string(),
        });
    }

    fn exit_crate(&mut self, _content: &mut Lintable<DocumentMut>) {
        let mut references = self.manifest.clone();
        for file in &self.files {
            references.push_str(&file.content);
        }

        for file in &self.files {
            let relative_path = file.path.strip_prefix(&self.crate_dir).unwrap();
            if is_cargo_target(relative_path)
                || file.content.contains(SUPPRESS)
                || is_referenced(relative_path, &references)
            {
                continue;
            }

            log::warn!(
                "{}: Rust source file is not referenced by a Cargo target, module, or include \
                 (add `{SUPPRESS}` to suppress)",
                file.path.display(),
            );
        }
    }

    fn exit_workspace(&mut self, _content: &mut Lintable<DocumentMut>) {}
}

fn is_cargo_target(path: &Path) -> bool {
    let components: Vec<_> = path.components().collect();
    match components.as_slice() {
        [file] if file.as_os_str() == "build.rs" => true,
        [src, file]
            if src.as_os_str() == "src"
                && matches!(file.as_os_str().to_str(), Some("lib.rs" | "main.rs")) =>
        {
            true
        }
        [directory, file]
            if matches!(
                directory.as_os_str().to_str(),
                Some("examples" | "tests" | "benches")
            ) && file.as_os_str().to_string_lossy().ends_with(".rs") =>
        {
            true
        }
        [src, bin, file]
            if src.as_os_str() == "src"
                && bin.as_os_str() == "bin"
                && file.as_os_str().to_string_lossy().ends_with(".rs") =>
        {
            true
        }
        [directory, _, main]
            if matches!(
                directory.as_os_str().to_str(),
                Some("examples" | "tests" | "benches")
            ) && main.as_os_str() == "main.rs" =>
        {
            true
        }
        [src, bin, _, main]
            if src.as_os_str() == "src"
                && bin.as_os_str() == "bin"
                && main.as_os_str() == "main.rs" =>
        {
            true
        }
        _ => false,
    }
}

fn is_referenced(path: &Path, references: &str) -> bool {
    let file_name = path.file_name().unwrap().to_string_lossy();
    if references.contains(file_name.as_ref()) {
        return true;
    }

    let module_name = if file_name == "mod.rs" {
        path.parent()
            .and_then(Path::file_name)
            .unwrap()
            .to_string_lossy()
    } else {
        path.file_stem().unwrap().to_string_lossy()
    };

    references.contains(&format!("mod {module_name};"))
        || references.contains(&format!("mod r#{module_name};"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_cargo_targets() {
        for path in [
            "build.rs",
            "src/lib.rs",
            "src/main.rs",
            "src/bin/tool.rs",
            "src/bin/tool/main.rs",
            "examples/demo.rs",
            "examples/demo/main.rs",
            "tests/integration.rs",
            "benches/benchmark.rs",
        ] {
            assert!(is_cargo_target(Path::new(path)), "{path}");
        }

        assert!(!is_cargo_target(Path::new("src/device.rs")));
        assert!(!is_cargo_target(Path::new("tests/common/mod.rs")));
    }

    #[test]
    fn recognizes_module_and_path_references() {
        assert!(is_referenced(
            Path::new("src/device.rs"),
            "pub(crate) mod device;"
        ));
        assert!(is_referenced(
            Path::new("src/device/mod.rs"),
            "pub mod device;"
        ));
        assert!(is_referenced(
            Path::new("src/templates/device.template.rs"),
            "include_str!(\"./templates/device.template.rs\")"
        ));
        assert!(!is_referenced(Path::new("src/device.rs"), "pub mod other;"));
    }
}
