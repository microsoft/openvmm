// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    // easier than repeating `cfg(any(..))` directives all over the place
    println!("cargo:rustc-check-cfg=cfg(with_encryption)");

    println!("cargo:rustc-cfg=with_encryption")
}
