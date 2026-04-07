// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Check for duplicate crate versions that can cause nondeterministic builds.
//!
//! When multiple versions of the same crate (by package name) are linked into
//! the final binary, LLVM's fat LTO pass may order them nondeterministically
//! across different build machines. This is only a problem for crate pairs
//! where the name collides — different major versions get different symbol
//! manglings and don't collide.
//!
//! This pass runs `cargo tree --depth 0 --duplicates` and flags any duplicate
//! that could cause reproducibility issues.

use super::FmtCtx;
use crate::shell::XtaskShell;
use crate::tasks::fmt::FmtPass;

pub struct DuplicateDeps;

impl FmtPass for DuplicateDeps {
    fn run(self, ctx: FmtCtx) -> anyhow::Result<()> {
        let FmtCtx {
            ctx: _,
            fix: _,
            only_diffed: _,
        } = ctx;

        let sh = XtaskShell::new()?;
        let output = sh
            .cmd("cargo")
            .args(["tree", "--depth", "0", "--duplicates"])
            .quiet()
            .read()?;

        // Parse the output. `cargo tree --depth 0 --duplicates` prints lines
        // like:
        //   base64 v0.13.1
        //   base64 v0.22.1
        //   bitflags v1.3.2
        //   bitflags v2.9.0
        //
        // We group by crate name and check for problematic duplicates.

        let mut crate_versions: std::collections::BTreeMap<
            String,
            std::collections::BTreeSet<String>,
        > = std::collections::BTreeMap::new();

        for line in output.lines() {
            let line = line.trim();
            // Only look at top-level crate lines (no leading tree characters)
            if let Some((name, version)) = parse_crate_line(line) {
                crate_versions
                    .entry(name.to_string())
                    .or_default()
                    .insert(version.to_string());
            }
        }

        let mut problems = Vec::new();

        // Crates that are known to have multiple versions and are acceptable.
        // These are typically different major versions (e.g., bitflags 1 vs 2)
        // where cargo gives them distinct symbol names, or crates that are
        // only used at build time. Add entries here only when the duplicate
        // cannot be reasonably unified.
        let allowed: std::collections::BTreeSet<&str> = [
            "bitflags",      // 1.x vs 2.x — different major, distinct symbols
            "fixedbitset",   // 0.4 vs 0.5 — pulled by petgraph versions
            "getrandom",     // 0.2 vs 0.3 — pulled by different rand ecosystems
            "heck",          // 0.4 vs 0.5 — pulled by derive macros
            "linux-raw-sys", // 0.4 vs 0.9 — pulled by different rustix versions
            "rustix",        // 0.38 vs 1.0 — ecosystem transition in progress
            "spin",          // 0.9 vs 0.10 — pulled by different lock crates
            "syn",           // 1.x vs 2.x — many proc-macro crates lag behind
            "thiserror",     // 1.x vs 2.x — ecosystem transition in progress
        ]
        .into_iter()
        .collect();

        for (name, versions) in &crate_versions {
            if versions.len() < 2 {
                continue;
            }

            if allowed.contains(name.as_str()) {
                continue;
            }

            // Any duplicate non-proc-macro crate with the same package name
            // can cause nondeterministic function ordering in LLVM's fat LTO
            // pass, regardless of how different the versions are.
            let versions_str: Vec<_> = versions.iter().map(|s| s.as_str()).collect();
            problems.push(format!("{name}: {}", versions_str.join(", ")));
        }

        if !problems.is_empty() {
            log::error!(
                "Found duplicate crate versions that may cause nondeterministic builds.\n\
                 Duplicate crates with the same effective major version can cause\n\
                 LLVM fat LTO to produce different binaries on different machines.\n\
                 \n\
                 Duplicates found:"
            );
            for problem in &problems {
                log::error!("  {problem}");
            }
            log::error!(
                "\nTo investigate, run: cargo tree --duplicates -i <crate>@<version>\n\
                 Fix by unifying transitive dependencies to a single version."
            );
            anyhow::bail!(
                "found {} problematic duplicate crate version(s)",
                problems.len()
            );
        }

        Ok(())
    }
}

/// Parse a line like `base64 v0.22.1` or `base64 v0.22.1 (proc-macro)` into
/// (name, version).
fn parse_crate_line(line: &str) -> Option<(&str, &str)> {
    // Skip lines that start with tree-drawing characters or whitespace
    // (those are dependency sub-entries, not top-level duplicates)
    if line.starts_with(|c: char| c.is_whitespace() || "│├└─".contains(c)) {
        return None;
    }

    let mut parts = line.split_whitespace();
    let name = parts.next()?;
    let version_str = parts.next()?;
    let version = version_str.strip_prefix('v')?;

    // Skip proc-macro crates — they don't contribute to runtime binary
    let rest: String = parts.collect();
    if rest.contains("(proc-macro)") {
        return None;
    }

    Some((name, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_crate_line() {
        assert_eq!(
            parse_crate_line("base64 v0.22.1"),
            Some(("base64", "0.22.1"))
        );
        assert_eq!(
            parse_crate_line("syn v1.0.109 (proc-macro)"),
            None // proc-macros are filtered
        );
        assert_eq!(
            parse_crate_line("├── serde v1.0.228"),
            None // tree sub-entries are filtered
        );
        assert_eq!(parse_crate_line(""), None);
    }
}
