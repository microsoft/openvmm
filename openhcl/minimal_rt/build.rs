// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    // Allow a cfg of nightly to avoid using a feature, see lib.rs.
    println!("cargo:rustc-check-cfg=cfg(nightly)");

    minimal_rt_build::init();
}
