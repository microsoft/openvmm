// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks host fingerprints against the pinned profile of their generation.
//!
//! ```text
//! cargo run -p cpu_profile --example check_fingerprint -- <fingerprint.json>...
//! ```
//!
//! Prints one `NVX-CPU-PROFILE:` line per fingerprint, and fails if any
//! check fails.

use cpu_profile::fingerprint::CpuFingerprint;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut status = ExitCode::SUCCESS;
    for path in std::env::args().skip(1) {
        let fingerprint = std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|json| CpuFingerprint::from_json(&json).map_err(|error| error.to_string()));
        match fingerprint {
            Ok(fingerprint) => {
                let check = cpu_profile::check_fingerprint(&fingerprint);
                println!("{path}: {}", check.summary_line(&fingerprint));
                if check.result.is_err() {
                    status = ExitCode::FAILURE;
                }
            }
            Err(error) => {
                println!("{path}: error: {error}");
                status = ExitCode::FAILURE;
            }
        }
    }
    status
}
