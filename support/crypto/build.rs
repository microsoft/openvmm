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
    println!("cargo::rerun-if-env-changed=CARGO_CFG_TARGET_ENV");

    println!("cargo::rustc-check-cfg=cfg(native)");
    println!("cargo::rustc-check-cfg=cfg(openssl)");
    println!("cargo::rustc-check-cfg=cfg(rust)");
    println!("cargo::rustc-check-cfg=cfg(symcrypt)");
    println!("cargo::rustc-check-cfg=cfg(single_backend)");

    let linux = std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "linux";
    let musl = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "musl";

    let native = std::env::var_os("CARGO_FEATURE_NATIVE").is_some();
    let openssl = std::env::var_os("CARGO_FEATURE_OPENSSL").is_some();
    let rust = std::env::var_os("CARGO_FEATURE_RUST").is_some();
    let symcrypt = std::env::var_os("CARGO_FEATURE_SYMCRYPT").is_some();
    let vendored = std::env::var_os("CARGO_FEATURE_VENDORED").is_some();

    let backend_count = openssl as u8 + rust as u8 + symcrypt as u8 + native as u8;

    // If no or multiple backends are enabled, fall back to the native backend
    // so that operations like `cargo check` and `cargo test` can succeed, but
    // emit a warning.
    if backend_count != 1 {
        if linux && musl {
            println!("cargo::rustc-cfg=symcrypt");
        } else if linux {
            println!("cargo::rustc-cfg=openssl");
        } else {
            println!("cargo::rustc-cfg=native");
        }
        println!(
            "cargo::warning=No or multiple crypto backends enabled. This may cause a link-time error."
        );
    }
    // If exactly one backend is enabled, use it and emit the `single_backend`
    // cfg for link-time checking. Don't emit the cfg for the rust backend,
    // as it's known to be insecure and should be used for testing purposes only.
    else if backend_count == 1 {
        let secure;
        if openssl {
            println!("cargo::rustc-cfg=openssl");
            secure = true;
        } else if symcrypt {
            if vendored {
                panic!("The symcrypt backend does not support vendoring");
            }
            println!("cargo::rustc-cfg=symcrypt");
            secure = true;
        } else if rust {
            println!("cargo::rustc-cfg=rust");
            secure = false;
        } else if native && linux && musl {
            println!("cargo::rustc-cfg=symcrypt");
            secure = true;
        } else if native && linux && !musl {
            println!("cargo::rustc-cfg=openssl");
            secure = true;
        } else if native && !linux {
            println!("cargo::rustc-cfg=native");
            secure = true;
        } else {
            unreachable!();
        }
        if secure {
            println!("cargo::rustc-cfg=single_backend");
        }
    }
}
