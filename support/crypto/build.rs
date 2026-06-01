// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_NATIVE");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_OPENSSL");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_RUST");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_SYMCRYPT");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_VENDORED");
    println!("cargo::rerun-if-env-changed=CARGO_CFG_TARGET_OS");

    println!("cargo::rustc-check-cfg=cfg(native)");
    println!("cargo::rustc-check-cfg=cfg(openssl)");
    println!("cargo::rustc-check-cfg=cfg(rust)");
    println!("cargo::rustc-check-cfg=cfg(symcrypt)");
    println!("cargo::rustc-check-cfg=cfg(multi_backend)");

    let linux = std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "linux";

    let native = std::env::var_os("CARGO_FEATURE_NATIVE").is_some();
    let openssl = std::env::var_os("CARGO_FEATURE_OPENSSL").is_some();
    let rust = std::env::var_os("CARGO_FEATURE_RUST").is_some();
    let symcrypt = std::env::var_os("CARGO_FEATURE_SYMCRYPT").is_some();
    let vendored = std::env::var_os("CARGO_FEATURE_VENDORED").is_some();

    let backend_count = openssl as u8 + rust as u8 + symcrypt as u8 + native as u8;

    // If no backends are enabled, abort. Binaries must choose a backend.
    if backend_count == 0 {
        panic!("No crypto backend enabled. Enable one in your binary's dependencies.");
    }
    // If exactly one backend is enabled, use it.
    else if backend_count == 1 {
        if openssl {
            println!("cargo::rustc-cfg=openssl");
        } else if symcrypt {
            if vendored {
                panic!("The symcrypt backend does not support vendoring");
            }
            println!("cargo::rustc-cfg=symcrypt");
        } else if rust {
            println!("cargo::rustc-cfg=rust");
        } else if native && linux {
            println!("cargo::rustc-cfg=openssl");
        } else if native && !linux {
            println!("cargo::rustc-cfg=native");
        }
    }
    // If multiple backends are enabled, fall back to the native backend so that
    // operations like `cargo check --workspace` can succeed, but emit a warning
    // and a cfg that will cause our link-time check to fail.
    else {
        if linux {
            println!("cargo::rustc-cfg=openssl");
        } else {
            println!("cargo::rustc-cfg=native");
        }
        println!("cargo::rustc-cfg=multi_backend");
    }
}
