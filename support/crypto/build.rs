// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    // Output a single config flag to indicate which backend should be used.
    // For now prefer OpenSSL if both backend features are enabled. This allows
    // compilation with --all-features to still succeed.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_OPENSSL");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SYMCRYPT");
    println!("cargo:rustc-check-cfg=cfg(native)");
    println!("cargo:rustc-check-cfg=cfg(openssl)");
    println!("cargo:rustc-check-cfg=cfg(symcrypt)");

    let openssl = std::env::var_os("CARGO_FEATURE_OPENSSL").is_some();
    let symcrypt = std::env::var_os("CARGO_FEATURE_SYMCRYPT").is_some();

    match (openssl, symcrypt) {
        (true, true) | (true, false) => println!("cargo:rustc-cfg=openssl"),
        (false, true) => println!("cargo:rustc-cfg=symcrypt"),
        (false, false) => println!("cargo:rustc-cfg=native"),
    }
}
