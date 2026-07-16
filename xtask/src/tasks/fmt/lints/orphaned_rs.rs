// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Warns about Rust source files that are not reachable from a Cargo target.

use super::Lint;
use super::LintCtx;
use super::Lintable;
use std::path::Path;
use std::path::PathBuf;
use toml_edit::DocumentMut;

pub struct OrphanedRustFiles {
    files: Vec<PathBuf>,
    references: String,
}

impl Lint for OrphanedRustFiles {
    fn new(_ctx: &LintCtx) -> Self {
        Self {
            files: Vec::new(),
            references: String::new(),
        }
    }

    fn enter_workspace(&mut self, _content: &Lintable<DocumentMut>) {}

    fn enter_crate(&mut self, _content: &Lintable<DocumentMut>) {
        self.files.clear();
        self.references.clear();
    }

    fn visit_file(&mut self, content: &mut Lintable<String>) {
        self.files.push(content.path().to_owned());
        self.references.push_str(content);
        self.references.push('\n');
    }

    fn exit_crate(&mut self, content: &mut Lintable<DocumentMut>) {
        let crate_dir = content.path().parent().unwrap_or(Path::new(""));
        let manifest = content.raw().unwrap_or_default();

        for file in &self.files {
            let relative_path = file.strip_prefix(crate_dir).unwrap();
            if is_cargo_target(relative_path)
                || is_referenced(relative_path, manifest)
                || is_referenced(relative_path, &self.references)
            {
                continue;
            }

            log::warn!(
                "{}: Rust source file is not referenced by a Cargo target, module, or include",
                file.display(),
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
        let Some(module_name) = path.parent().and_then(Path::file_name) else {
            return false;
        };
        module_name.to_string_lossy()
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

    #[test]
    fn crate_root_mod_rs_is_unreferenced() {
        assert!(!is_referenced(Path::new("mod.rs"), ""));
    }
}
