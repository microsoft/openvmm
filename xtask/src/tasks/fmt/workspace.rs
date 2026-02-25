// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use crate::shell::XtaskShell;
use anyhow::Context;
use clap::Parser;
use rayon::prelude::*;
use serde::Deserialize;
use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use toml_edit::Item;
use toml_edit::TableLike;
use toml_edit::Value;

#[derive(Parser)]
#[clap(about = "Verify that all Cargo.toml files are valid and in the workspace")]
pub struct VerifyWorkspace;

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

impl Xtask for VerifyWorkspace {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let excluded = {
            // will always be root Cargo.toml, as xtasks run from project root
            let contents = fs_err::read_to_string("Cargo.toml")?;
            let parsed = contents.parse::<toml_edit::DocumentMut>()?;

            if let Some(excluded) = parsed
                .as_table()
                .get("workspace")
                .and_then(|w| w.get("exclude"))
                .and_then(|e| e.as_array())
            {
                let mut exclude = Vec::new();
                for entry in excluded {
                    let entry = entry.as_str().unwrap();
                    exclude.push(
                        std::path::absolute(entry)
                            .with_context(|| format!("cannot exclude {}", entry))?,
                    );
                }
                exclude
            } else {
                Vec::new()
            }
        };

        // Find directory entries.
        let entries = ignore::WalkBuilder::new(ctx.root)
            .filter_entry(move |e| {
                for path in excluded.iter() {
                    if e.path().starts_with(path) {
                        return false;
                    }
                }

                true
            })
            .build()
            .filter_map(|entry| match entry {
                Ok(entry) if entry.file_name() == "Cargo.toml" => Some(entry.into_path()),
                Err(err) => {
                    log::error!("error when walking over subdirectories: {}", err);
                    None
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        let manifests = workspace_manifests()?;

        let all_present = entries.iter().all(|entry| {
            if !manifests.contains(entry) {
                log::error!("Error: {} is not present in the workspace", entry.display());
                false
            } else {
                true
            }
        });

        let dependencies_valid = manifests.par_iter().all(|entry| {
            if let Err(err) = verify_dependencies(entry) {
                log::error!("Error: failed to verify {}: {:#}", entry.display(), err);
                false
            } else {
                true
            }
        });

        if !all_present || !dependencies_valid {
            anyhow::bail!("found invalid Cargo.toml");
        }

        Ok(())
    }
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<Package>,
    workspace_root: PathBuf,
}

#[derive(Deserialize)]
struct Package {
    manifest_path: PathBuf,
}

fn workspace_manifests() -> anyhow::Result<HashSet<PathBuf>> {
    let json = XtaskShell::new()?
        .cmd("cargo")
        .arg("metadata")
        .arg("--no-deps")
        .arg("--format-version=1")
        .read()?;
    let metadata: CargoMetadata =
        serde_json::from_str(&json).context("failed to parse JSON result")?;

    Ok(metadata
        .packages
        .into_iter()
        .map(|p| p.manifest_path)
        .chain([metadata.workspace_root.join("Cargo.toml")])
        .collect())
}

fn verify_dependencies(path: &PathBuf) -> Result<(), anyhow::Error> {
    // TODO: Convert this to a better crate like cargo_toml once it supports inherited dependencies fully.
    let contents = fs_err::read_to_string(path)?;
    let parsed = contents.parse::<toml_edit::DocumentMut>()?;

    let package_name = match parsed
        .as_table()
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    {
        Some(name) => name,
        None => return Ok(()), // Workspace root toml
    };

    let mut dep_tables = Vec::new();
    for (name, v) in parsed.iter() {
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

    let found_bad_deps = Cell::new(false);

    let handle_non_workspaced_dep = |dep_name| {
        let allowed = WORKSPACE_EXCEPTIONS
            .iter()
            .find_map(|&(p, crates)| (p == package_name).then_some(crates))
            .unwrap_or(&[]);

        if allowed.contains(&dep_name) {
            log::debug!(
                "{} contains non-workspaced dependency {}. Allowed by exception.",
                package_name,
                dep_name
            );
        } else {
            found_bad_deps.set(true);
            log::error!(
                "{} contains non-workspaced dependency {}. Please move this dependency to the root Cargo.toml.",
                package_name,
                dep_name
            );
        }
    };
    let check_table_like = |t: &dyn TableLike, dep_name| {
        if t.get("workspace").and_then(|x| x.as_bool()) != Some(true) {
            handle_non_workspaced_dep(dep_name);
        }
    };

    for table in dep_tables {
        for (dep_name, value) in table.iter() {
            match value {
                Item::Value(Value::String(_)) => handle_non_workspaced_dep(dep_name),
                Item::Value(Value::InlineTable(t)) => {
                    check_table_like(t, dep_name);

                    if t.len() == 1 {
                        found_bad_deps.set(true);
                        log::error!(
                            "{} uses inline table syntax for its dependency on {}, but only contains one table entry. Please change to the dotted syntax.",
                            package_name,
                            dep_name
                        );
                    }
                }
                Item::Table(t) => check_table_like(t, dep_name),

                _ => unreachable!(),
            }
        }
    }

    if found_bad_deps.get() {
        Err(anyhow::anyhow!("Found incorrectly defined dependencies."))
    } else {
        Ok(())
    }
}
