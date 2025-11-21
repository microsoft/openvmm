// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use std::env;

fn main() {
    // Include debug build info if INCLUDE_DEBUG_BUILD_INFO is set in the environment
    // Automatically set by local `cargo xflowey build-igvm` workflows
    let include_debug_info = env::var("INCLUDE_DEBUG_BUILD_INFO").is_ok();
    if include_debug_info {
        println!("cargo:rustc-env=INCLUDE_DEBUG_BUILD_INFO=1");

        let timestamp = time::OffsetDateTime::now_local();
        if let Ok(timestamp) = timestamp {
            println!(
                "cargo:rustc-env=SOURCE_DATE_EPOCH={}",
                timestamp
                    .format(&time::format_description::well_known::Rfc2822)
                    .expect("failed to format timestamp")
            );
        }

        let host = hostname::get()
            .unwrap()
            .into_string()
            .expect("hostname was not a valid string");
        println!("cargo:rustc-env=DEBUG_BUILD_INFO_MACHINE_NAME={host}");

        let sh = xshell::Shell::new().unwrap();

        // Check for uncommitted changes
        let has_changes = !xshell::cmd!(sh, "git status --porcelain")
            .read()
            .expect("failed to execute git status command")
            .trim()
            .is_empty();

        if has_changes {
            println!("cargo:rustc-env=DEBUG_BUILD_INFO_UNCOMMITTED_CHANGES=true");
        } else {
            println!("cargo:rustc-env=DEBUG_BUILD_INFO_UNCOMMITTED_CHANGES=false");
        }
    }

    vergen::EmitBuilder::builder().all_git().emit().unwrap();
}
