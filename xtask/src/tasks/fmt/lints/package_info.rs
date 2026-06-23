// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks to ensure that the `[package]` sections of Cargo.toml files do not
//! contain `authors` or `version` fields, and that rust-version, edition, and
//! fields are properly workspaced.
//!
//! Eliding the [version][] sets the version to "0.0.0", which is fine. More
//! importantly, it means the module cannot be published to crates.io
//! (equivalent to publish = false), which is what we want for our internal
//! crates. And removing the meaningless version also eliminates more questions
//! from newcomers (does the version field mean anything? do we use it for
//! semver internally?).
//!
//! The [authors][] field is optional, is not really used anywhere anymore, and
//! just creates confusion.
//!
//! [version]:
//!     <https://doc.rust-lang.org/cargo/reference/manifest.html#the-version-field>
//! [authors]:
//!     <https://doc.rust-lang.org/cargo/reference/manifest.html#the-authors-field>

use super::Lint;
use super::LintCtx;
use super::Lintable;
use toml_edit::DocumentMut;
use toml_edit::Item;
use toml_edit::Table;

/// List of packages that are allowed to have a version
static VERSION_EXCEPTIONS: &[&str] = &["vmgstool"];

pub struct PackageInfo;

impl Lint for PackageInfo {
    fn new(_ctx: &LintCtx) -> Self {
        PackageInfo
    }

    fn enter_workspace(&mut self, _content: &Lintable<DocumentMut>) {}
    fn enter_crate(&mut self, _content: &Lintable<DocumentMut>) {}
    fn visit_file(&mut self, _content: &mut Lintable<String>) {}

    fn exit_crate(&mut self, content: &mut Lintable<DocumentMut>) {
        let package = content["package"].as_table().unwrap();
        let excluded_from_workspace = package
            .get("metadata")
            .and_then(|x| x.get("xtask"))
            .and_then(|x| x.get("house-rules"))
            .and_then(|x| x.get("excluded-from-workspace"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let package_name = package["name"].as_str().unwrap();
        let check_version = !VERSION_EXCEPTIONS.contains(&package_name);

        let mut lints_table = Table::new();
        lints_table.insert("workspace", Item::Value(true.into()));

        let mut rust_version_field = Table::new();
        rust_version_field.set_dotted(true);
        rust_version_field.insert("workspace", Item::Value(true.into()));

        let mut edition_field = Table::new();
        edition_field.set_dotted(true);
        edition_field.insert("workspace", Item::Value(true.into()));

        let has_authors = package.contains_key("authors");
        let has_version = check_version && package.contains_key("version");
        let needs_lints_fix = !excluded_from_workspace
            && content.get("lints").map(|o| o.to_string()).as_deref()
                != Some(&lints_table.to_string());
        let needs_rust_version_fix = !excluded_from_workspace
            && package
                .get("rust-version")
                .map(|o| o.to_string())
                .as_deref()
                != Some(&rust_version_field.to_string());
        let needs_edition_fix = !excluded_from_workspace
            && package.get("edition").map(|o| o.to_string()).as_deref()
                != Some(&edition_field.to_string());

        if has_authors {
            content.fix("package should not have authors field", |doc| {
                doc["package"].as_table_mut().unwrap().remove("authors");
            });
        }

        if has_version {
            content.fix("package should not have version field", |doc| {
                doc["package"].as_table_mut().unwrap().remove("version");
            });
        }

        if needs_lints_fix {
            content.fix("lints should be workspaced", |doc| {
                doc.insert("lints", Item::Table(lints_table));
            });
        }

        if needs_rust_version_fix {
            content.fix("rust-version should be workspaced", |doc| {
                doc["package"]
                    .as_table_mut()
                    .unwrap()
                    .insert("rust-version", Item::Table(rust_version_field));
            });
        }

        if needs_edition_fix {
            content.fix("edition should be workspaced", |doc| {
                doc["package"]
                    .as_table_mut()
                    .unwrap()
                    .insert("edition", Item::Table(edition_field));
            });
        }
    }

    fn exit_workspace(&mut self, _content: &mut Lintable<DocumentMut>) {}
}
