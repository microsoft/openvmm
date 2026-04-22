// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    // Output a single config flag to indicate which backend should be used.
    // For now prefer OpenSSL if both backend features are enabled. This allows
    // compilation with --all-features to still succeed.
    println!("cargo:rustc-check-cfg=cfg(native,openssl,symcrypt)");
    match (cfg!(feature = "openssl"), cfg!(feature = "symcrypt")) {
        (true, true) | (true, false) => println!("cargo:rustc-cfg=openssl"),
        (false, true) => println!("cargo:rustc-cfg=symcrypt"),
        (false, false) => println!("cargo:rustc-cfg=native"),
    }
}
