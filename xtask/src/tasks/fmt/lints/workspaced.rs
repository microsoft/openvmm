// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks that every crate's Cargo.toml is properly workspaced.

use super::Lint;
use super::LintCtx;
use super::Lintable;
use std::path::Path;
use std::path::PathBuf;
use toml_edit::DocumentMut;
use toml_edit::Item;
use toml_edit::TableLike;
use toml_edit::Value;

/// List of exceptions to using workspace package declarations.
static WORKSPACE_EXCEPTIONS: &[(&str, &[&str])] = &[
    // Allow disk_blob to use tokio for now, but no one else.
    //
    // disk_blob eventually will remove its tokio dependency.
    ("disk_blob", &["tokio"]),
    // Allow mesh_rpc to use tokio, since h2 depends on it for the tokio IO
    // trait definitions. Hopefully this can be resolved upstream once async IO
    // trait "vocabulary types" move to a common crate.
    ("mesh_rpc", &["tokio"]),
];

pub struct WorkspacedManifest {
    members: Vec<PathBuf>,
    excluded: Vec<PathBuf>,
    dependencies: Vec<PathBuf>,
}

impl Lint for WorkspacedManifest {
    fn new(_ctx: &LintCtx) -> Self {
        WorkspacedManifest {
            members: Vec::new(),
            excluded: Vec::new(),
            dependencies: Vec::new(),
        }
    }

    fn enter_workspace(&mut self, content: &Lintable<DocumentMut>) {
        // Gather the set of crates we expect to see: all members, dependencies, and exclusions
        self.members = content["workspace"]
            .get("members")
            .and_then(|m| m.as_array())
            .into_iter()
            .flat_map(|a| a.into_iter())
            .map(|m| Path::new(m.as_str().unwrap()).join("Cargo.toml"))
            .collect();
        self.excluded = content["workspace"]
            .get("exclude")
            .and_then(|e| e.as_array())
            .into_iter()
            .flat_map(|a| a.into_iter())
            .map(|e| Path::new(e.as_str().unwrap()).join("Cargo.toml"))
            .collect();
        self.dependencies = content["workspace"]
            .get("dependencies")
            .and_then(|d| d.as_table())
            .into_iter()
            .flat_map(|t| t.into_iter())
            // We only need to keep local dependencies, external dependencies don't get visited
            .filter_map(|(_k, v)| {
                v.get("path")
                    .map(|p| Path::new(p.as_str().unwrap()).join("Cargo.toml"))
            })
            .collect();
    }

    fn enter_crate(&mut self, content: &Lintable<DocumentMut>) {
        // Remove this crate from whichever set it appears in, but ensure it only appears in one
        let mut count = 0;
        if let Some(member) = self.members.iter().position(|m| content.path() == m) {
            self.members.remove(member);
            count += 1;
        }
        if let Some(excluded) = self.excluded.iter().position(|e| content.path() == e) {
            self.excluded.remove(excluded);
            count += 1;
        }
        if let Some(dependency) = self.dependencies.iter().position(|d| content.path() == d) {
            self.dependencies.remove(dependency);
            count += 1;
        }

        if count == 0 {
            content.unfixable("crate is not a workspace member, dependency, or exclusion");
        } else if count > 1 {
            content.unfixable("crate appears in multiple workspace sections");
        }
    }

    fn visit_file(&mut self, _content: &mut Lintable<String>) {}

    fn exit_crate(&mut self, content: &mut Lintable<DocumentMut>) {
        // Verify that all dependencies of this crate are workspaced
        let mut dep_tables = Vec::new();
        for (name, v) in content.iter() {
            match name {
                "dependencies" | "build-dependencies" | "dev-dependencies" => {
                    dep_tables.push(v.as_table_like().unwrap())
                }
                "target" => {
                    let flattened = v
                        .as_table_like()
                        .unwrap()
                        .iter()
                        .flat_map(|(_, v)| v.as_table_like().unwrap().iter());

                    for (k, v) in flattened {
                        match k {
                            "dependencies" | "build-dependencies" | "dev-dependencies" => {
                                dep_tables.push(v.as_table_like().unwrap())
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        let crate_name = content["package"]["name"].as_str().unwrap();
        let handle_bad_dep = |dep_name| {
            let allowed = WORKSPACE_EXCEPTIONS
                .iter()
                .find_map(|&(p, crates)| (p == crate_name).then_some(crates))
                .unwrap_or(&[]);

            if allowed.contains(&dep_name) {
                log::debug!(
                    "{} contains non-workspaced dependency {}. Allowed by exception.",
                    content.path().display(),
                    dep_name
                );
            } else {
                content.unfixable(&format!("non-workspaced dependency {} found", dep_name));
            }
        };
        let check_table_like = |t: &dyn TableLike, dep_name| {
            if t.get("workspace").and_then(|x| x.as_bool()) != Some(true) {
                handle_bad_dep(dep_name);
            }
        };

        for table in dep_tables {
            for (dep_name, value) in table.iter() {
                match value {
                    Item::Value(Value::String(_)) => handle_bad_dep(dep_name),
                    Item::Value(Value::InlineTable(t)) => {
                        check_table_like(t, dep_name);

                        if t.len() == 1 {
                            content.unfixable(&format!(
                                "inline table syntax used for dependency on {} but only one table entry is present, change to the dotted form",
                                dep_name
                            ));
                        }
                    }
                    Item::Table(t) => check_table_like(t, dep_name),
                    _ => unreachable!(),
                }
            }
        }
    }

    fn exit_workspace(&mut self, content: &mut Lintable<DocumentMut>) {
        // Any members or dependencies that we expected to see but didn't are errors
        for member in self.members.iter() {
            content.unfixable(&format!(
                "workspace member {} does not exist",
                member.display()
            ));
        }
        for dependency in self.dependencies.iter() {
            // TODO: Remove this exception once xsync no longer depends on ci_logger
            if dependency == Path::new("../support/ci_logger/Cargo.toml") {
                continue;
            }
            content.unfixable(&format!(
                "workspace dependency {} does not exist",
                dependency.display()
            ));
        }
        // Exclusions that we didn't see may be nested workspaces, which don't get visited, so they're allowed
    }
}
