// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    // Allow a cfg of nightly to avoid using a feature, see main.rs.
    println!("cargo:rustc-check-cfg=cfg(nightly)");
}
