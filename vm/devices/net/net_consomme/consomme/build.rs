// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build script for consomme package.

fn main() {
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rustc-link-lib=resolv");
    }

    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-lib=resolv");
    }
}
