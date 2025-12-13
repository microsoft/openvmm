// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build script for consomme package.

fn main() {
    #[cfg(not(target_os = "windows"))]
    {
        println!("cargo:rustc-link-lib=resolv");
    }
}
