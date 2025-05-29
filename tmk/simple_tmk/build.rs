// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build script for the simple TMK, needed to build with `minimal_rt`.

fn main() {
    if minimal_rt_build::init() {
        // Needed to preserve the `tmk_tests` section for enumerating
        // tests.
        println!("cargo:rustc-link-arg=-znostart-stop-gc");
    }
}
