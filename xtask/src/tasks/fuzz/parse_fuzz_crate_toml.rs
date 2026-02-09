// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Logic to parse + validate fuzzing-crate `Cargo.toml` files + their
//! associated folder structure.

use anyhow::Context;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug)]
pub(super) struct FuzzCrateTarget {
    pub name: String,
    pub allowlist: Vec<PathBuf>,
    pub target_options: Vec<String>,
}

#[derive(Debug)]
pub(super) struct FuzzCrateMetadata {
    pub crate_name: String,
    pub fuzz_dir: PathBuf,
    pub targets: Vec<FuzzCrateTarget>,
}

#[derive(Debug)]
pub(super) struct RepoFuzzTarget {
    pub fuzz_dir: PathBuf,
    #[expect(dead_code)] // useful in `dump` debug output
    pub crate_name: String,
    pub allowlist: Vec<PathBuf>,
    pub target_options: Vec<String>,
}

// TODO: it would be nice if this function didn't early-bail when it detects an
// error, and instead keeps validating the file (as best as it can)
fn parse_fuzz_crate_toml(cargo_toml_path: &Path) -> anyhow::Result<Option<FuzzCrateMetadata>> {
    let manifest =
        cargo_toml::Manifest::<super::cargo_package_metadata::PackageMetadata>::from_path_with_metadata(
            cargo_toml_path,
        )?;

    // Check if the crate is a OpenVMM-style cargo-fuzz crate
    let fuzz_meta = {
        // ...and simultaneously make sure crates that _aren't_ OpenVMM-style
        // cargo-fuzz crates don't misconstrue themselves as such
        let validate_non_fuzz_crate_name = || {
            let name = manifest
                .package
                .as_ref()
                .map(|x| x.name())
                .unwrap_or_default();

            if name.starts_with("fuzz_") {
                anyhow::bail!("crate '{name}' is named 'fuzz_', but isn't set up to be a fuzzer!")
            }

            anyhow::Ok(None)
        };

        // If the crate doesn't have _any_ metadata, it's def not a fuzzing crate
        let Some(metadata) = manifest.package.as_ref().and_then(|p| p.metadata.as_ref()) else {
            return validate_non_fuzz_crate_name();
        };

        // Check to make sure fuzz crates include both the "standard" cargo-fuzz
        // metadata, and OpenVMM-specific metadata.
        match (
            metadata.cargo_fuzz.unwrap_or(false),
            metadata.xtask.as_ref().and_then(|x| x.fuzz.as_ref()),
        ) {
            (false, None) => return validate_non_fuzz_crate_name(),
            (true, None) | (false, Some(_)) => {
                anyhow::bail!(
                    "`package.metadata.cargo-fuzz` must be paired with `package.metadata.xtask.fuzz`"
                )
            }
            (true, Some(fuzz)) => fuzz,
        }
    };

    // cool, we're in a fuzz crate!

    // make sure the fuzz crate is within a directory called "fuzz". this isn't
    // _strictly_ necessary (`cargo fuzz` supports passing a custom fuzz
    // directory), but the consistency is nice.
    if cargo_toml_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|p| p.to_str())
        .unwrap_or_default()
        != "fuzz"
    {
        anyhow::bail!("fuzzing crate Cargo.toml must be in a folder called `fuzz/`")
    }

    // make sure our fuzz crate naming is consistent, to make it easy to tell
    // what is / isn't a fuzzing crate.
    let fuzz_crate_name = manifest
        .package
        .as_ref()
        .map(|p| p.name.as_str())
        .unwrap_or_default();
    let Some(fuzz_crate_name) = fuzz_crate_name.strip_prefix("fuzz_") else {
        anyhow::bail!(r#"fuzzing crate `name` must start with "fuzz_""#)
    };

    // make sure that [[bin]] is structured correctly
    let mut bins = BTreeSet::new();
    for bin in manifest.bin {
        let name = bin
            .name
            .context("found [[bin]] entry without explicit `name` key")?;

        if bin.path.is_none() {
            anyhow::bail!(r#"found [[bin]] entry (name = {name}) without explicit `path` key"#,)
        }

        // this isn't just for consistency! it also helps make fuzz target name
        // collisions across the tree far less likely.
        if !(name == format!("fuzz_{fuzz_crate_name}")
            || name.starts_with(&format!("fuzz_{fuzz_crate_name}_")))
        {
            anyhow::bail!(
                r#"invalid [[bin]] entry: invalid name = "{name}". expected `name` to start with "fuzz_{fuzz_crate_name}" (i.e: "fuzz_{{crate_name}}")"#,
            )
        }

        for (val, name) in [
            (bin.test, "test"),
            (bin.doctest, "doctest"),
            (bin.doc, "doc"),
        ] {
            if val {
                anyhow::bail!(r#"invalid [[bin]] entry: ensure that `{name} = false`"#)
            }
        }

        let was_empty = bins.insert(name);
        assert!(was_empty); // cargo guarantees that there are no dupes
    }

    // ensure there is a 1:1 match between each allowlist entry and bin entry
    {
        let mut allowlists = fuzz_meta.allowlist.keys().cloned().collect::<BTreeSet<_>>();
        for bin in bins.iter() {
            let was_present = allowlists.remove(bin);
            if !was_present {
                anyhow::bail!("found [[bin]] that doesn't have an allowlist: {bin}")
            }
        }
        if !allowlists.is_empty() {
            anyhow::bail!(
                "found allowlist entries that doesn't corresponding [[bin]] entries: {allowlists:?}"
            )
        }
    }

    let targets = {
        let mut targets = Vec::new();
        for (target_name, allowlist) in &fuzz_meta.allowlist {
            // normalize allowlist globs using `glob` to avoid taking a
            // dependance on the funky OneFuzz allowlist format
            let mut normalized_allowlist = Vec::new();
            let mut normalized_ignorelist = BTreeSet::new();

            let (allowed_globs, ignored_globs): (Vec<_>, Vec<_>) =
                allowlist.iter().partition(|s| !s.starts_with('!'));

            let normalize_glob = |glob: &str| {
                let mut normalized_paths = Vec::new();

                let anchored_glob = cargo_toml_path.parent().unwrap().join(glob);
                let paths = glob::glob(&anchored_glob.to_string_lossy())
                    .context(format!("'{target_name}' has invalid allowlist glob format"))?;

                for path in paths {
                    let path =
                        std::path::absolute(path?).context("failed to make path absolute")?;

                    if path.is_dir() {
                        continue;
                    }

                    normalized_paths.push(path);
                }

                anyhow::Ok(normalized_paths)
            };

            for glob in ignored_globs {
                normalized_ignorelist.extend(normalize_glob(glob.strip_prefix('!').unwrap())?)
            }

            for glob in allowed_globs {
                for path in normalize_glob(glob)? {
                    if !normalized_ignorelist.contains(&path) {
                        normalized_allowlist.push(path)
                    }
                }
            }

            if normalized_allowlist.is_empty() {
                anyhow::bail!("'{target_name}' has allowlist that matches no files")
            }

            targets.push(FuzzCrateTarget {
                name: target_name.clone(),
                allowlist: normalized_allowlist,
                target_options: fuzz_meta
                    .target_options
                    .get(target_name)
                    .cloned()
                    .unwrap_or_default(),
            })
        }
        targets
    };

    let fuzz_crate_metadata = FuzzCrateMetadata {
        crate_name: manifest.package.as_ref().map(|x| x.name.clone()).unwrap(),
        fuzz_dir: cargo_toml_path.parent().unwrap().into(),
        targets,
    };

    Ok(Some(fuzz_crate_metadata))
}

pub(super) fn get_repo_fuzz_crates(
    ctx: &crate::XtaskCtx,
) -> anyhow::Result<Vec<FuzzCrateMetadata>> {
    let cargo_tomls = ignore::Walk::new(&ctx.root).filter_map(|entry| match entry {
        Ok(entry) if entry.file_name() == "Cargo.toml" => Some(entry.into_path()),
        Err(err) => {
            log::error!("error when walking over subdirectories: {}", err);
            None
        }
        _ => None,
    });

    let mut fuzz_crates = Vec::new();
    let mut errors = Vec::new();
    for path in cargo_tomls {
        match parse_fuzz_crate_toml(&path) {
            Ok(None) => {}
            Ok(Some(meta)) => fuzz_crates.push(meta),
            Err(e) => errors.push(e.context(format!("in {}", path.display()))),
        }
    }

    if !errors.is_empty() {
        for e in &errors {
            log::error!("{:#}", e);
        }
        anyhow::bail!("failed to verify in-tree fuzzers")
    }

    Ok(fuzz_crates)
}

pub(super) fn get_repo_fuzz_targets(
    fuzz_crates: &[FuzzCrateMetadata],
) -> anyhow::Result<BTreeMap<String, RepoFuzzTarget>> {
    let mut fuzz_targets = BTreeMap::new();
    for FuzzCrateMetadata {
        fuzz_dir,
        targets,
        crate_name,
    } in fuzz_crates
    {
        // if two fuzz crates happen to have the same fuzz target name,
        // whichever crate happens to be built last will override the crate
        // built earlier, which is very bad.
        for FuzzCrateTarget {
            name,
            allowlist,
            target_options,
        } in targets
        {
            let existing = fuzz_targets.insert(
                name.clone(),
                RepoFuzzTarget {
                    crate_name: crate_name.clone(),
                    fuzz_dir: fuzz_dir.clone(),
                    allowlist: allowlist.clone(),
                    target_options: target_options.clone(),
                },
            );

            if let Some(existing) = existing {
                anyhow::bail!(
                    "cannot have two targets with the same name: {} (in {} and {})",
                    name,
                    fuzz_dir.display(),
                    existing.fuzz_dir.display()
                )
            }
        }
    }

    Ok(fuzz_targets)
}
