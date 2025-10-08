// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use std::path::Path;

fn main() {
    // Prevent this build script from rerunning unnecessarily.
    println!("cargo:rerun-if-changed=build.rs");

    if cfg!(windows) {
        // From the cargo docs at
        // <https://doc.rust-lang.org/cargo/reference/environment-variables.html#dynamic-library-paths>
        //
        // > Search paths included from any build script with the rustc-link-search
        // > instruction. Paths outside of the target directory are removed. It is
        // > the responsibility of the user running Cargo to properly set the
        // > environment if additional libraries on the system are needed in the
        // > search path.
        //
        // While it's understandable why cargo decided to adopt this policy
        // (wouldn't want some random third-party crate to rely on hard-coded system
        // paths for DLLs!), it kinda stinks for our use-cases!
        //
        // To make life easier for devs working on this code, we do a sneaky
        // maneuver and copy the dll right into the `OUT_DIR`, thereby ensuring
        // `cargo run` Just Works.

        // xtask-fmt allow-target-arch sys-crate
        let deps_dir = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
            "x86_64" => std::env::var("X86_64_LXUTIL_DLL_DIR").unwrap(),
            "aarch64" => std::env::var("AARCH64_LXUTIL_DLL_DIR").unwrap(),
            _ => panic!("unsupported architecture"),
        };

        let out_dir = std::env::var("OUT_DIR").unwrap();
        std::fs::copy(
            Path::new(&deps_dir).join("lxutil.dll"),
            Path::new(&out_dir).join("lxutil.dll"),
        )
        .unwrap();

        println!("cargo:rustc-link-search={}", out_dir);
    }
}
