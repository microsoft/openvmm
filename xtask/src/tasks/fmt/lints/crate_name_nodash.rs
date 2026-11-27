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

        // Check for allow-dash-in-name metadata escape hatch. Lookups work on
        // both standard and inline TOML tables.
        if content
            .as_table()
            .get("package")
            .and_then(|x| x.get("metadata"))
            .and_then(|x| x.get("xtask"))
            .and_then(|x| x.get("house-rules"))
            .and_then(|x| x.get("allow-dash-in-name"))
            .and_then(|x| x.as_bool())
            .unwrap_or(false)
        {
            return;
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

#[cfg(test)]
mod tests {
    use super::super::test_support::failed;
    use super::super::test_support::lintable;
    use super::*;

    fn fails(path: &str, manifest: &str) -> bool {
        let mut file = lintable(path, manifest.parse::<DocumentMut>().unwrap(), false);
        CrateNameNoDash.exit_crate(&mut file);
        failed(&file)
    }

    // Test: Check crate names and folders with and without dashes, the boolean
    //       escape hatch set true or false, and a workspace-only manifest.
    // Expected: Any dash in the name or folder fails unless explicitly allowed;
    //       manifests without a package are skipped.
    #[test]
    fn rejects_dashes_unless_explicitly_allowed() {
        assert!(!fails(
            "vm/good_crate/Cargo.toml",
            "[package]\nname = \"good_crate\"\n"
        ));
        assert!(fails(
            "vm/good_crate/Cargo.toml",
            "[package]\nname = \"bad-crate\"\n"
        ));
        assert!(fails(
            "vm/bad-dir/Cargo.toml",
            "[package]\nname = \"good\"\n"
        ));
        assert!(fails(
            "vm/bad-dir/Cargo.toml",
            "[package]\nname = \"bad-name\"\n"
        ));
        let allow = |v: &str| {
            format!(
                "[package]\nname = \"bad-name\"\n[package.metadata.xtask.house-rules]\nallow-dash-in-name = {v}\n"
            )
        };
        assert!(!fails("vm/bad-name/Cargo.toml", &allow("true")));
        assert!(fails("vm/bad-name/Cargo.toml", &allow("false")));
        assert!(!fails("Cargo.toml", "[workspace]\nmembers = []\n"));
    }

    // Test: Check a dashed crate whose escape hatch is an inline TOML table,
    //       and one whose `house-rules` value is not a table at all.
    // Expected: The inline form is honoured like the standard table form; a
    //       non-table value grants no exception and is reported, without panicking.
    #[test]
    fn inline_table_escape_hatch_is_honoured() {
        let manifest = "[package]\nname = \"bad-name\"\nmetadata.xtask.house-rules = { allow-dash-in-name = true }\n";
        assert!(!fails("vm/bad-name/Cargo.toml", manifest));
        let not_table = "[package]\nname = \"bad-name\"\nmetadata.xtask.house-rules = true\n";
        assert!(fails("vm/bad-name/Cargo.toml", not_table));
    }
}
