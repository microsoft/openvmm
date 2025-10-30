// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Check for unused Rust dependencies
//!
//! Forked from <https://github.com/bnjbvr/cargo-machete>
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

use crate::Xtask;
use anyhow::Context;
use clap::Parser;
use grep_regex::RegexMatcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::BinaryDetection;
use grep_searcher::Searcher;
use grep_searcher::SearcherBuilder;
use grep_searcher::Sink;
use grep_searcher::SinkMatch;
use rayon::prelude::*;
use std::error;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Parser)]
#[clap(about = "Detect any unused dependencies in Cargo.toml files")]
#[clap(after_help = r#"NOTE:

    False-positives can be suppressed by setting `package.metadata.xtask.unused-dep.ignored`
    in the corresponding `Cargo.toml` file.

    For example, "test-env-log" has implicit deps on both "env_logger" and "tracing-subscriber":

        [package.metadata.xtask.unused-deps]
        ignored = ["env_logger", "tracing-subscriber"]
"#)]
pub struct UnusedDeps {
    /// Attempt to remove any unused dependencies from Cargo.toml files.
    #[clap(long)]
    pub fix: bool,
}

impl Xtask for UnusedDeps {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        // Find directory entries.
        let entries = ignore::Walk::new(&ctx.root)
            .filter_map(|entry| match entry {
                Ok(entry) => {
                    if entry.file_name() == "Cargo.toml" {
                        Some(entry.into_path())
                    } else {
                        None
                    }
                }
                Err(err) => {
                    log::error!("error when walking over subdirectories: {}", err);
                    None
                }
            })
            .collect::<Vec<_>>();

        // Run analysis in parallel. This will spawn new rayon tasks when dependencies are effectively
        // used by any Rust crate.
        let mut results = entries
            .par_iter()
            .filter_map(|path| match analyze_crate(path) {
                Ok(Some(analysis)) => Some((analysis, path)),

                Ok(None) => {
                    log::debug!("{} is a virtual manifest for a workspace", path.display());
                    None
                }

                Err(err) => {
                    log::error!("error when handling {}: {}", path.display(), err);
                    None
                }
            })
            .collect::<Vec<_>>();

        results.sort_by(|a, b| a.1.cmp(b.1));

        let mut workspace = analyze_workspace(&ctx.root)?;
        let full_deps = workspace.deps.clone();

        // Display all the results.

        let mut found_something = false;
        for (analysis, path) in results {
            if !analysis.results.is_empty() {
                found_something = true;
                println!("{} -- {}:", analysis.package_name, path.display());
                for result in &analysis.results {
                    match result {
                        DepResult::Unused(n) => println!("\t{} is unused", n),
                        DepResult::IgnoredButUsed(n) => {
                            println!("\t{} is ignored, but being used", n)
                        }
                        DepResult::IgnoredAndMissing(n) => {
                            println!("\t{} is ignored, but it's not even being depended on", n)
                        }
                    }
                }

                if self.fix {
                    let fixed =
                        remove_dependencies(&fs_err::read_to_string(path)?, &analysis.results)?;
                    fs_err::write(path, fixed).context("Cargo.toml write error")?;
                }
            }

            workspace.deps.retain(|x| !analysis.deps.contains(x));
        }

        workspace.deps.sort();
        workspace.ignored.sort();
        if workspace.deps != workspace.ignored {
            found_something = true;
            let mut unused_deps = Vec::new();

            println!("Workspace -- {}:", workspace.path.display());
            for dep in &workspace.deps {
                if !workspace.ignored.contains(dep) {
                    println!("\t{} is unused", dep);
                    unused_deps.push(DepResult::Unused(dep.clone()));
                }
            }
            for ign in &workspace.ignored {
                if !workspace.deps.contains(ign) {
                    if full_deps.contains(ign) {
                        println!("\t{} is ignored, but being used", ign);
                        unused_deps.push(DepResult::IgnoredButUsed(ign.clone()));
                    } else {
                        println!("\t{} is ignored, but it's not even being depended on", ign);
                        unused_deps.push(DepResult::IgnoredAndMissing(ign.clone()));
                    }
                }
            }

            if self.fix {
                let fixed =
                    remove_dependencies(&fs_err::read_to_string(&workspace.path)?, &unused_deps)?;
                fs_err::write(&workspace.path, fixed).context("Cargo.toml write error")?;
            }
        }

        if found_something && !self.fix {
            Err(anyhow::anyhow!("found dependency issues"))
        } else {
            Ok(())
        }
    }
}

fn remove_dependencies(manifest: &str, analysis_results: &[DepResult]) -> anyhow::Result<String> {
    let mut manifest = toml_edit::DocumentMut::from_str(manifest)?;

    let mut unused_deps = Vec::new();
    let mut ignored_and_shouldnt_be = Vec::new();

    for res in analysis_results {
        match res {
            DepResult::Unused(n) => unused_deps.push(n),
            DepResult::IgnoredButUsed(n) => ignored_and_shouldnt_be.push(n),
            DepResult::IgnoredAndMissing(n) => ignored_and_shouldnt_be.push(n),
        }
    }

    let mut features_table = None;
    let mut dep_tables = Vec::new();
    let mut ignored_array = None;
    for (k, v) in manifest.iter_mut() {
        let v = match v {
            v if v.is_table_like() => v.as_table_like_mut().unwrap(),
            _ => continue,
        };

        match k.get() {
            "dependencies" | "build-dependencies" | "dev-dependencies" => dep_tables.push(v),
            "target" => {
                let flattened = v.iter_mut().flat_map(|(_, v)| {
                    v.as_table_like_mut()
                        .expect("conforms to cargo schema")
                        .iter_mut()
                });

                for (k, v) in flattened {
                    let v = match v {
                        v if v.is_table_like() => v.as_table_like_mut().unwrap(),
                        _ => continue,
                    };

                    match k.get() {
                        "dependencies" | "build-dependencies" | "dev-dependencies" => {
                            dep_tables.push(v)
                        }
                        _ => {}
                    }
                }
            }
            "workspace" => {
                for (k2, v2) in v.iter_mut() {
                    let v2 = match v2 {
                        v2 if v2.is_table_like() => v2.as_table_like_mut().unwrap(),
                        _ => continue,
                    };

                    match k2.get() {
                        "dependencies" => dep_tables.push(v2),
                        "metadata" => {
                            // get_mut() seems to create a new table that wasn't previously
                            // there in some cases, so first check with the immutable
                            // accessors.
                            if v2
                                .get("xtask")
                                .and_then(|x| x.get("unused-deps"))
                                .and_then(|u| u.get("ignored"))
                                .is_some()
                            {
                                ignored_array = v2
                                    .get_mut("metadata")
                                    .unwrap()
                                    .get_mut("xtask")
                                    .unwrap()
                                    .get_mut("unused-deps")
                                    .unwrap()
                                    .get_mut("ignored")
                                    .unwrap()
                                    .as_array_mut();
                            }
                        }
                        _ => {}
                    }
                }
            }
            "package" => {
                // get_mut() seems to create a new table that wasn't previously
                // there in some cases, so first check with the immutable
                // accessors.
                if v.get("metadata")
                    .and_then(|m| m.get("xtask"))
                    .and_then(|x| x.get("unused-deps"))
                    .and_then(|u| u.get("ignored"))
                    .is_some()
                {
                    ignored_array = v
                        .get_mut("metadata")
                        .unwrap()
                        .get_mut("xtask")
                        .unwrap()
                        .get_mut("unused-deps")
                        .unwrap()
                        .get_mut("ignored")
                        .unwrap()
                        .as_array_mut();
                }
            }
            "features" => features_table = Some(v),
            _ => {}
        }
    }

    for i in ignored_and_shouldnt_be {
        let ignored_array = ignored_array
            .as_mut()
            .expect("must have an ignored array for IgnoredButUsed results to appear");
        let index = ignored_array
            .iter()
            .position(|v| v.as_str() == Some(i))
            .expect("must find items that were found in previous pass");
        ignored_array.remove(index);
    }

    if let Some(features_table) = features_table {
        for (_feature_name, feature_deps) in features_table.iter_mut() {
            let mut to_remove = Vec::new();
            let feature_deps = feature_deps
                .as_array_mut()
                .expect("feature dependencies must be an array");
            for index in 0..feature_deps.len() {
                let feature_dep_name = feature_deps
                    .get(index)
                    .unwrap()
                    .as_str()
                    .expect("feature dependencies must be strings");
                let feature_dep_name = feature_dep_name
                    .strip_prefix("dep:")
                    .unwrap_or(feature_dep_name);
                for unused in &unused_deps {
                    if feature_dep_name.starts_with(&**unused)
                        && (feature_dep_name.len() == unused.len()
                            || matches!(feature_dep_name.as_bytes()[unused.len()], b'/' | b'?'))
                    {
                        to_remove.push(index);
                    }
                }
            }
            for i in to_remove.into_iter().rev() {
                feature_deps.remove(i);
            }
        }
    }

    for dep_table in dep_tables {
        unused_deps.retain(|dep| dep_table.remove(dep).is_none());
    }
    assert!(unused_deps.is_empty());

    let serialized = manifest.to_string();
    Ok(serialized)
}

mod meta {
    use serde::Deserialize;
    use serde::Serialize;

    #[derive(Serialize, Deserialize)]
    pub struct PackageMetadata {
        pub xtask: Option<Xtask>,
    }
    #[derive(Serialize, Deserialize)]
    pub struct Xtask {
        #[serde(rename = "unused-deps")]
        pub unused_deps: Option<Ignored>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Ignored {
        pub ignored: Vec<String>,
    }
}

type Manifest = cargo_toml::Manifest<meta::PackageMetadata>;

struct PackageAnalysis {
    pub package_name: String,
    pub results: Vec<DepResult>,
    pub deps: Vec<String>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum DepResult {
    /// Dependency is unused and not marked as ignored.
    Unused(String),
    /// Dependency is marked as ignored but used.
    IgnoredButUsed(String),
    /// Dependency is marked as ignored but not being depended on.
    IgnoredAndMissing(String),
}

struct WorkspaceAnalysis {
    pub path: PathBuf,
    pub deps: Vec<String>,
    pub ignored: Vec<String>,
}

fn make_regexp(name: &str) -> String {
    // Breaking down this regular expression: given a line,
    // - `use (::)?{name}(::|;| as)`: matches `use foo;`, `use foo::bar`, `use foo as bar;`, with
    // an optional "::" in front of the crate's name.
    // - `\b({name})::`: matches `foo::X`, but not `barfoo::X`. `\b` means word boundary, so
    // putting it before the crate's name ensures there's no polluting prefix.
    // - `extern crate {name}( |;)`: matches `extern crate foo`, or `extern crate foo as bar`.
    format!(r#"use (::)?{name}(::|;| as)|\b{name}::|extern crate {name}( |;)"#)
}

/// Returns all the paths to the Rust source files for a crate contained at the given path.
fn collect_paths(dir_path: &Path, manifest: &Manifest) -> Vec<PathBuf> {
    let mut root_paths = Vec::new();

    if let Some(path) = manifest.lib.as_ref().and_then(|lib| lib.path.as_ref()) {
        assert!(
            path.ends_with(".rs"),
            "paths provided by cargo_toml are to Rust files"
        );
        let mut path_buf = PathBuf::from(path);
        // Remove .rs extension.
        path_buf.pop();
        root_paths.push(path_buf);
    }

    for product in (manifest.bin.iter())
        .chain(manifest.bench.iter())
        .chain(manifest.test.iter())
        .chain(manifest.example.iter())
    {
        if let Some(ref path) = product.path {
            assert!(
                path.ends_with(".rs"),
                "paths provided by cargo_toml are to Rust files"
            );
            let mut path_buf = PathBuf::from(path);
            // Remove .rs extension.
            path_buf.pop();
            root_paths.push(path_buf);
        }
    }

    log::trace!("found root paths: {:?}", root_paths);

    if root_paths.is_empty() {
        // Assume "src/" if cargo_toml didn't find anything.
        root_paths.push(PathBuf::from("src"));
        log::trace!("adding src/ since paths was empty");
    }

    // Collect all final paths for the crate first.
    let mut paths: Vec<PathBuf> = root_paths
        .iter()
        .flat_map(|root| ignore::Walk::new(dir_path.join(root)))
        .filter_map(|result| {
            let dir_entry = match result {
                Ok(dir_entry) => dir_entry,
                Err(err) => {
                    log::error!("{}", err);
                    return None;
                }
            };

            if !dir_entry.file_type().unwrap().is_file() {
                return None;
            }

            if dir_entry
                .path()
                .extension()
                .is_none_or(|ext| ext.to_str() != Some("rs"))
            {
                return None;
            }

            Some(dir_entry.path().to_owned())
        })
        .collect();

    let build_rs = dir_path.join("build.rs");
    if build_rs.exists() {
        paths.push(build_rs);
    }

    log::trace!("found transitive paths: {:?}", paths);

    paths
}

struct Search {
    matcher: RegexMatcher,
    searcher: Searcher,
    sink: StopAfterFirstMatch,
}

impl Search {
    fn new(crate_name: &str) -> anyhow::Result<Self> {
        let snaked = crate_name.replace('-', "_");
        let pattern = make_regexp(&snaked);
        let matcher = RegexMatcherBuilder::new()
            .multi_line(true)
            .build(&pattern)?;

        let searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .line_number(false)
            .build();

        let sink = StopAfterFirstMatch::new();

        Ok(Self {
            matcher,
            searcher,
            sink,
        })
    }

    fn search_path(&mut self, path: &Path) -> anyhow::Result<bool> {
        self.searcher
            .search_path(&self.matcher, path, &mut self.sink)
            .map_err(|err| anyhow::anyhow!("when searching: {}", err))
            .map(|_| self.sink.found)
    }
}

fn analyze_workspace(root: &Path) -> anyhow::Result<WorkspaceAnalysis> {
    let path = root.join("Cargo.toml");
    let manifest = Manifest::from_path_with_metadata(&path)?;
    let workspace = manifest
        .workspace
        .expect("workspace manifest must have a workspace section");

    let deps = workspace.dependencies.into_keys().collect();

    let ignored = workspace
        .metadata
        .and_then(|meta| meta.xtask.and_then(|x| x.unused_deps.map(|u| u.ignored)))
        .unwrap_or_default();

    Ok(WorkspaceAnalysis {
        deps,
        path,
        ignored,
    })
}

fn analyze_crate(manifest_path: &Path) -> anyhow::Result<Option<PackageAnalysis>> {
    let mut dir_path = manifest_path.to_path_buf();
    dir_path.pop();

    log::trace!("trying to open {}...", manifest_path.display());

    let mut manifest = Manifest::from_path_with_metadata(manifest_path)?;
    let package_name = match manifest.package {
        Some(ref package) => package.name.clone(),
        None => return Ok(None),
    };

    log::debug!("handling {} ({})", package_name, dir_path.display());

    manifest.complete_from_path(manifest_path)?;

    let paths = collect_paths(&dir_path, &manifest);

    let mut deps = Vec::new();

    deps.extend(manifest.dependencies.keys().cloned());
    deps.extend(manifest.build_dependencies.keys().cloned());
    deps.extend(manifest.dev_dependencies.keys().cloned());
    for target in manifest.target.iter() {
        deps.extend(target.1.dependencies.keys().cloned());
        deps.extend(target.1.build_dependencies.keys().cloned());
        deps.extend(target.1.dev_dependencies.keys().cloned());
    }

    let ignored = if let Some(unused_deps) = manifest
        .package
        .and_then(|package| package.metadata)
        .and_then(|meta| meta.xtask.and_then(|x| x.unused_deps))
    {
        unused_deps.ignored
    } else {
        Vec::new()
    };

    let mut results = deps
        .par_iter()
        .filter_map(|name| {
            let mut search = Search::new(name).expect("constructing grep context");

            let mut found_once = false;
            for path in &paths {
                log::trace!("looking for {} in {}", name, path.to_string_lossy());
                match search.search_path(path) {
                    Ok(true) => {
                        found_once = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(err) => {
                        log::error!("{}: {}", path.display(), err);
                    }
                };
            }

            let ignored = ignored.contains(name);

            match (found_once, ignored) {
                (true, true) => Some(DepResult::IgnoredButUsed(name.into())),
                (true, false) => None,
                (false, true) => None,
                (false, false) => Some(DepResult::Unused(name.into())),
            }
        })
        .collect::<Vec<_>>();

    for i in &ignored {
        if !deps.contains(i) {
            results.push(DepResult::IgnoredAndMissing(i.clone()));
        }
    }

    results.sort();

    Ok(Some(PackageAnalysis {
        package_name,
        results,
        deps,
    }))
}

struct StopAfterFirstMatch {
    found: bool,
}

impl StopAfterFirstMatch {
    fn new() -> Self {
        Self { found: false }
    }
}

impl Sink for StopAfterFirstMatch {
    type Error = Box<dyn error::Error>;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let mat = String::from_utf8(mat.bytes().to_vec())?;
        let mat = mat.trim();

        if (mat.starts_with("//") || mat.starts_with("//!"))
            && !(mat.starts_with("/// # use") || mat.starts_with("/// use"))
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
