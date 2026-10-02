// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Derives a CPU profile from host fingerprints.
//!
//! ```text
//! cargo run -p cpu_profile --example derive_profile -- <generation> <revision> <fingerprint.json>...
//! ```
//!
//! Writes the profile, as pretty canonical JSON, to standard output, and its
//! ID and digest to standard error. Pin it under
//! `vmm_core/cpu_profile/profiles/<id>.json`, regenerate the static data in
//! `src/pinned_data.rs` with the `generate_pinned` example, and add the file
//! and its golden digest to the catalog test's `FILES` table.

use cpu_profile::derive;
use cpu_profile::fingerprint::CpuFingerprint;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: derive_profile <generation> <revision> <fingerprint.json>...";
    let name = args.next().ok_or(usage)?;
    let revision = args.next().ok_or(usage)?.parse::<u32>()?;
    let generation = derive::known_generation(&name).ok_or_else(|| {
        format!(
            "unknown generation {name:?}; known: {}",
            derive::KNOWN_GENERATIONS
                .iter()
                .map(|generation| generation.name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let fingerprints = args
        .map(|path| {
            let json =
                std::fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))?;
            CpuFingerprint::from_json(&json).map_err(|error| format!("{path}: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let profile = derive::derive_profile(generation, revision, &fingerprints)?;
    print!("{}", profile.to_pretty_json());
    eprintln!("{} {}", profile.id(), profile.digest_string());
    Ok(())
}
