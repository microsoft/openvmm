// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Check for unused Rust dependencies
//!
//! Based upon <https://github.com/bnjbvr/cargo-machete>
//! (license copied in source)

// Copyright (c) 2022 Benjamin Bouvier
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use super::Lint;
use super::LintCtx;
use super::Lintable;
use grep_regex::RegexMatcher;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use toml_edit::DocumentMut;

pub struct UnusedDeps {
    /// All dependency names declared in the workspace.
    workspace_deps: BTreeSet<String>,
    /// Workspace dependencies for which a usage has been found so far in source files.
    workspace_found_deps: BTreeSet<String>,

    /// All dependency names declared in the current crate.
    crate_deps: BTreeSet<String>,
    /// Dependencies for which a usage has been found so far in source files.
    crate_found_deps: BTreeSet<String>,

    /// Pre-compiled regex matchers keyed by dependency name, built once for
    /// all workspace dependency names and extended lazily for any
    /// non-workspace deps encountered in individual crates.
    dep_matchers: BTreeMap<String, RegexMatcher>,

    /// Whether we are linting only diffed files (workspace-level check is
    /// unreliable in this mode because not all crates are visited).
    only_diffed: bool,

    /// Prebuilt grep searcher to avoid rebuilds
    searcher: grep_searcher::Searcher,
}

impl Lint for UnusedDeps {
    fn new(ctx: &LintCtx) -> Self {
        UnusedDeps {
            workspace_deps: BTreeSet::new(),
            workspace_found_deps: BTreeSet::new(),
            crate_deps: BTreeSet::new(),
            crate_found_deps: BTreeSet::new(),
            dep_matchers: BTreeMap::new(),
            only_diffed: ctx.only_diffed,
            searcher: grep_searcher::SearcherBuilder::new()
                .line_number(false)
                .build(),
        }
    }

    fn enter_workspace(&mut self, content: &Lintable<DocumentMut>) {
        // Extract workspace dependency names.
        if let Some(deps) = content
            .get("workspace")
            .and_then(|w| w.get("dependencies"))
            .and_then(|d| d.as_table_like())
        {
            // Pre-compile regex matchers for every workspace dependency.
            for (dep_name, _) in deps.iter() {
                self.workspace_deps.insert(dep_name.to_string());
                self.dep_matchers
                    .insert(dep_name.to_string(), compile_matcher(dep_name));
            }
        }
    }

    fn enter_crate(&mut self, content: &Lintable<DocumentMut>) {
        const DEP_TABLE_NAMES: &[&str] =
            &["dependencies", "build-dependencies", "dev-dependencies"];

        self.crate_deps.clear();
        self.crate_found_deps.clear();

        for dep_table in DEP_TABLE_NAMES
            .iter()
            .flat_map(|s| content.get(s).map(|d| d.as_table_like().unwrap()))
        {
            for (dep_name, _) in dep_table.iter() {
                self.crate_deps.insert(dep_name.to_string());
                if !self.dep_matchers.contains_key(dep_name) {
                    self.dep_matchers
                        .insert(dep_name.to_string(), compile_matcher(dep_name));
                }
            }
        }

        // Target-specific dependencies.
        if let Some(target) = content.get("target").map(|t| t.as_table_like().unwrap()) {
            for target_table in target.iter().map(|(_, t)| t.as_table_like().unwrap()) {
                for dep_table in DEP_TABLE_NAMES
                    .iter()
                    .flat_map(|s| target_table.get(s).map(|d| d.as_table_like().unwrap()))
                {
                    for (dep_name, _) in dep_table.iter() {
                        self.crate_deps.insert(dep_name.to_string());
                        if !self.dep_matchers.contains_key(dep_name) {
                            self.dep_matchers
                                .insert(dep_name.to_string(), compile_matcher(dep_name));
                        }
                    }
                }
            }
        }
    }

    fn visit_file(&mut self, content: &mut Lintable<String>) {
        if self.crate_found_deps.len() == self.crate_deps.len() {
            return;
        }

        let unfound = self.crate_deps.difference(&self.crate_found_deps);
        let mut found = Vec::new();
        for looking in unfound {
            let needle = looking.replace('-', "_");
            // Fast rejection: if the identifier doesn't appear at all, the regex can't match.
            if !content.contains(&needle) {
                continue;
            }
            // ... run regex only for candidates that pass the pre-filter
            let mut sink = StopAfterFirstMatch::new();
            self.searcher
                .search_slice(
                    &self.dep_matchers[&**looking],
                    content.as_bytes(),
                    &mut sink,
                )
                .unwrap();
            if sink.found {
                found.push(looking.clone());
            }
        }

        self.crate_found_deps.extend(found.iter().cloned());
        self.workspace_found_deps.extend(found);
    }

    fn exit_crate(&mut self, content: &mut Lintable<DocumentMut>) {
        exit_toml(content, &self.crate_deps, &self.crate_found_deps, false);
    }

    fn exit_workspace(&mut self, content: &mut Lintable<DocumentMut>) {
        // When only diffed files are being checked we haven't visited every
        // crate, so the used_workspace_deps set is incomplete.  Skip the
        // workspace-level check to avoid false positives.
        if self.only_diffed {
            return;
        }

        exit_toml(
            content,
            &self.workspace_deps,
            &self.workspace_found_deps,
            true,
        );
    }
}

fn exit_toml(
    content: &mut Lintable<DocumentMut>,
    deps: &BTreeSet<String>,
    found_deps: &BTreeSet<String>,
    is_workspace: bool,
) {
    let top_key = if is_workspace { "workspace" } else { "package" };
    // Extract per-manifest ignored list.
    let ignored: BTreeSet<_> = content
        .get(top_key)
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("xtask"))
        .and_then(|x| x.get("unused-deps"))
        .and_then(|u| u.get("ignored"))
        .map(|arr| {
            arr.as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();

    for ign in &ignored {
        if !deps.contains(ign) {
            let msg = format!("{ign} is ignored, but it's not being depended on");
            content.fix(&msg, |doc| remove_from_ignored(doc, ign, is_workspace));
        }
    }

    for dep in deps {
        let found = found_deps.contains(&**dep);
        let ignored = ignored.contains(&**dep);

        match (found, ignored) {
            (false, false) => {
                content.fix(&format!("{dep} is unused"), |doc| {
                    remove_from_deps(doc, dep, is_workspace)
                });
            }
            (true, true) => {
                content.fix(&format!("{dep} is ignored, but being used"), |doc| {
                    remove_from_ignored(doc, dep, is_workspace)
                });
            }
            _ => {}
        }
    }
}

fn compile_matcher(dep: &str) -> RegexMatcher {
    let name = dep.replace('-', "_");
    // Syntax documentation: https://docs.rs/regex/latest/regex/#syntax
    //
    // Breaking down this regular expression: given a line,
    // - `use (::)?{name}(::|;| as)`: matches `use foo;`, `use foo::bar`, `use foo as bar;`, with
    // an optional "::" in front of the crate's name.
    // - `(?:[^:]|^|\W::)\b{name}::`: matches `foo::X`, but not `barfoo::X`. To ensure there's no polluting
    //   prefix we add `(?:[^:]|^|\W::)\b`, meaning that the crate name must be prefixed by either:
    //    * Not a `:` (therefore not a sub module)
    //    * The start of a line
    //    * Not a word character followed by `::` (to allow ::my_crate)
    // - `extern crate {name}( |;)`: matches `extern crate foo`, or `extern crate foo as bar`.
    // - `{name}` makes the match against the crate's name case insensitive
    let regex =
        format!(r#"use (::)?{name}(::|;| as)|(?:[^:]|^|\W::)\b{name}::|extern crate {name}( |;)"#);
    RegexMatcher::new_line_matcher(&regex).unwrap()
}

/// Remove a dependency from all dep tables and its references in `[features]`.
fn remove_from_deps(doc: &mut DocumentMut, dep_name: &str, is_workspace: bool) {
    const DEP_TABLE_NAMES: &[&str] = &["dependencies", "build-dependencies", "dev-dependencies"];

    if is_workspace {
        // Remove from [workspace.dependencies]
        let deps = doc["workspace"]["dependencies"]
            .as_table_like_mut()
            .unwrap();
        deps.remove(dep_name);
    } else {
        // Remove from root-level dep tables.
        for table_name in DEP_TABLE_NAMES {
            if let Some(deps) = doc.get_mut(table_name).and_then(|d| d.as_table_like_mut()) {
                deps.remove(dep_name);
            }
        }

        // Remove from target-specific dep tables.
        if let Some(target) = doc.get_mut("target").and_then(|t| t.as_table_like_mut()) {
            let keys: Vec<String> = target.iter().map(|(k, _)| k.to_string()).collect();
            for key in keys {
                if let Some(target_table) = target.get_mut(&key).and_then(|t| t.as_table_like_mut())
                {
                    for table_name in DEP_TABLE_NAMES {
                        if let Some(deps) = target_table
                            .get_mut(table_name)
                            .and_then(|d| d.as_table_like_mut())
                        {
                            deps.remove(dep_name);
                        }
                    }
                }
            }
        }

        // Remove references from [features].
        if let Some(features) = doc.get_mut("features").and_then(|f| f.as_table_like_mut()) {
            let dep_enable = format!("dep:{dep_name}");
            let dep_feature_prefix = format!("{dep_name}/");
            let feature_keys: Vec<String> = features.iter().map(|(k, _)| k.to_string()).collect();
            for feature_key in feature_keys {
                if let Some(arr) = features
                    .get_mut(&feature_key)
                    .and_then(|v| v.as_array_mut())
                {
                    arr.retain(|v| {
                        if let Some(s) = v.as_str() {
                            s != dep_enable && !s.starts_with(&dep_feature_prefix)
                        } else {
                            true
                        }
                    });
                }
            }
        }
    }
}

/// Remove a name from the `ignored` metadata array
fn remove_from_ignored(doc: &mut DocumentMut, dep_name: &str, is_workspace: bool) {
    let top_key = if is_workspace { "workspace" } else { "package" };
    let ignored_array = doc[top_key]["metadata"]["xtask"]["unused-deps"]["ignored"]
        .as_array_mut()
        .unwrap();
    ignored_array.retain(|v| v.as_str() != Some(dep_name));
}

struct StopAfterFirstMatch {
    found: bool,
}

impl StopAfterFirstMatch {
    fn new() -> Self {
        Self { found: false }
    }
}

impl grep_searcher::Sink for StopAfterFirstMatch {
    type Error = Box<dyn std::error::Error>;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        let mat = mat.bytes().trim_ascii();

        if mat.starts_with(b"//")
            && !(mat.starts_with(b"/// # use")
                || mat.starts_with(b"/// use")
                || mat.starts_with(b"//! # use")
                || mat.starts_with(b"//! use"))
        {
            // Continue if seeing what resembles a comment or an inner doc
            // comment.
            // Certain exceptions for outer doc comments (`///`) because they may contain code
            // examples that use dependencies, and skipping them could cause us to miss usages
            // relevant for dependency detection. Unfortunately we can't check whether the example
            // is within a code snippet without actually parsing the code.
            return Ok(true);
        }

        // Otherwise, we've found it: mark to true, and return false to indicate that we can stop
        // searching.
        self.found = true;
        Ok(false)
    }
}
