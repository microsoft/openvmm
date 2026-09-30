// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Warns about Rust source files that are not reachable from a Cargo target.

use super::Lint;
use super::LintCtx;
use super::Lintable;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use toml_edit::DocumentMut;

#[derive(Default)]
struct References {
    file_names: HashSet<String>,
    module_names: HashSet<String>,
}

pub struct OrphanedRustFiles {
    files: Vec<PathBuf>,
    references: References,
}

impl Lint for OrphanedRustFiles {
    fn new(_ctx: &LintCtx) -> Self {
        Self {
            files: Vec::new(),
            references: References::default(),
        }
    }

    fn enter_workspace(&mut self, _content: &Lintable<DocumentMut>) {}

    fn enter_crate(&mut self, _content: &Lintable<DocumentMut>) {
        self.files.clear();
        self.references.clear();
    }

    fn visit_file(&mut self, content: &mut Lintable<String>) {
        self.files.push(content.path().to_owned());
        self.references.extend(content);
    }

    fn exit_crate(&mut self, content: &mut Lintable<DocumentMut>) {
        let crate_dir = content.path().parent().unwrap_or(Path::new(""));
        let manifest = content.raw().unwrap_or_default();
        self.references.extend(manifest);

        for file in &self.files {
            let relative_path = file.strip_prefix(crate_dir).unwrap();
            if is_cargo_target(relative_path) || self.references.contains(relative_path) {
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

impl References {
    fn clear(&mut self) {
        self.file_names.clear();
        self.module_names.clear();
    }

    fn extend(&mut self, content: &str) {
        let mut remaining = content;
        while let Some(index) = remaining.find(".rs") {
            let end = index + ".rs".len();
            let start = remaining[..index]
                .char_indices()
                .rfind(|(_, character)| !is_file_name_character(*character))
                .map_or(0, |(index, character)| index + character.len_utf8());
            self.file_names.insert(remaining[start..end].to_owned());
            remaining = &remaining[end..];
        }

        for line in content.lines() {
            let mut remaining = line;
            while let Some(index) = remaining.find("mod ") {
                remaining = &remaining[index + "mod ".len()..];
                let Some(end) = remaining.find(';') else {
                    break;
                };
                let module_name = &remaining[..end];
                if !module_name.is_empty()
                    && !module_name.chars().any(char::is_whitespace)
                    && module_name
                        .chars()
                        .all(|character| is_file_name_character(character) || character == '#')
                {
                    self.module_names.insert(
                        module_name
                            .strip_prefix("r#")
                            .unwrap_or(module_name)
                            .to_owned(),
                    );
                }
                remaining = &remaining[end + 1..];
            }
        }
    }

    fn contains(&self, path: &Path) -> bool {
        let file_name = path.file_name().unwrap().to_string_lossy();
        if self.file_names.contains(file_name.as_ref()) {
            return true;
        }

        self.module_names.contains(module_name(path).as_ref())
    }
}

fn is_file_name_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-' | '.')
}

fn module_name(path: &Path) -> std::borrow::Cow<'_, str> {
    let file_name = path.file_name().unwrap().to_string_lossy();
    if file_name == "mod.rs" {
        let Some(module_name) = path.parent().and_then(Path::file_name) else {
            return "".into();
        };
        module_name.to_string_lossy()
    } else {
        path.file_stem().unwrap().to_string_lossy()
    }
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
        let mut references = References::default();
        references.extend(
            r#"
            //! Documentation about `mod service`.
            pub(crate) mod device;
            pub mod r#type;
            include_str!("./templates/device.template.rs");
            "#,
        );

        assert!(references.contains(Path::new("src/device.rs")));
        assert!(references.contains(Path::new("src/device/mod.rs")));
        assert!(references.contains(Path::new("src/type.rs")));
        assert!(references.contains(Path::new("src/templates/device.template.rs")));
        assert!(!references.contains(Path::new("src/other.rs")));
    }

    #[test]
    fn crate_root_mod_rs_is_unreferenced() {
        let references = References::default();
        assert!(!references.contains(Path::new("mod.rs")));
    }
}
