// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build script for consomme package.

fn main() {
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("windows") => {}
        Ok("macos") | Ok("linux") => println!("cargo:rustc-link-lib=resolv"),
        _ => {}
    }
}
