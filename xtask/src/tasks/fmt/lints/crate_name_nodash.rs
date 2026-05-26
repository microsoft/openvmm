// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks that crate names and their containing folder names do not use
//! dashes (hyphens). Use underscores instead.

use super::Lint;
use super::LintCtx;
use super::Lintable;
use toml_edit::DocumentMut;

pub struct CrateNameNoDash;

impl Lint for CrateNameNoDash {
    fn new(_ctx: &LintCtx) -> Self {
        CrateNameNoDash
    }

    fn enter_workspace(&mut self, _content: &Lintable<DocumentMut>) {}

    fn enter_crate(&mut self, _content: &Lintable<DocumentMut>) {}

    fn visit_file(&mut self, _content: &mut Lintable<String>) {}

    fn exit_crate(&mut self, content: &mut Lintable<DocumentMut>) {
        let package_name = match content
            .as_table()
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
        {
            Some(name) => name,
            None => return, // Root of a different workspace, like xsync
        };

        // Check for allow-dash-in-name metadata escape hatch.
        if let Some(metadata) = content
            .as_table()
            .get("package")
            .and_then(|x| x.get("metadata"))
            .and_then(|x| x.get("xtask"))
            .and_then(|x| x.get("house-rules"))
        {
            let props = metadata.as_table().unwrap();
            for (k, v) in props.iter() {
                if k == "allow-dash-in-name" {
                    if v.as_bool().unwrap_or(false) {
                        return;
                    }
                }
            }
        }

        let bad_package_name = package_name.contains('-');
        let bad_package_path = content
            .path()
            .parent()
            .and_then(|p| p.file_name())
            .unwrap_or_default()
            .to_string_lossy()
            .contains('-');

        let msg = match (bad_package_name, bad_package_path) {
            (true, true) => "crate name + folder cannot contain '-' char",
            (true, false) => "crate name cannot contain '-' char",
            (false, true) => "crate folder cannot contain '-' char",
            _ => return,
        };

        content.unfixable(&format!(
            "{}: name={} folder={}",
            msg,
            package_name,
            content
                .path()
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ));
    }

    fn exit_workspace(&mut self, _content: &mut Lintable<DocumentMut>) {}
}
