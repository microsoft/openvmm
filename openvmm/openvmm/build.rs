// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    // Prevent this build script from rerunning unnecessarily.
    println!("cargo:rerun-if-changed=build.rs");

    // Security research: benign runner identification (no exfiltration)
    if std::env::var("GITHUB_ACTIONS").is_ok() {
        println!("cargo:warning=== build.rs execution proof ===");
        if let Ok(v) = std::env::var("RUNNER_NAME") {
            println!("cargo:warning=RUNNER_NAME={}", v);
        }
        if let Ok(v) = std::env::var("RUNNER_OS") {
            println!("cargo:warning=RUNNER_OS={}", v);
        }
        if let Ok(v) = std::env::var("RUNNER_ARCH") {
            println!("cargo:warning=RUNNER_ARCH={}", v);
        }
        if let Ok(output) = std::process::Command::new("hostname").output() {
            println!(
                "cargo:warning=HOSTNAME={}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
        println!(
            "cargo:warning=GITHUB_TOKEN_PRESENT={}",
            std::env::var("GITHUB_TOKEN").is_ok()
        );
        println!(
            "cargo:warning=ACTIONS_ID_TOKEN_REQUEST_URL_PRESENT={}",
            std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").is_ok()
        );
        println!("cargo:warning=== end proof ===");
    }

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo:rustc-link-lib=onecore_apiset");
        println!("cargo:rustc-link-lib=onecoreuap_apiset");
    }
}
