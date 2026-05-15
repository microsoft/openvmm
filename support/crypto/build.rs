// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_OPENSSL");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_RUST");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_SYMCRYPT");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_VENDORED");
    println!("cargo::rerun-if-env-changed=CARGO_CFG_TARGET_OS");

    println!("cargo::rustc-check-cfg=cfg(native)");
    println!("cargo::rustc-check-cfg=cfg(openssl)");
    println!("cargo::rustc-check-cfg=cfg(rust)");
    println!("cargo::rustc-check-cfg=cfg(symcrypt)");

    let openssl = std::env::var_os("CARGO_FEATURE_OPENSSL").is_some();
    let rust = std::env::var_os("CARGO_FEATURE_RUST").is_some();
    let symcrypt = std::env::var_os("CARGO_FEATURE_SYMCRYPT").is_some();
    let vendored = std::env::var_os("CARGO_FEATURE_VENDORED").is_some();
    let linux = std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "linux";

    let all_features = openssl && rust && symcrypt && vendored;
    let backend_count = openssl as u8 + rust as u8 + symcrypt as u8;

    // If we see multiple backends enabled that's an error. However if we see every
    // backend, and vendoring, enabled, it's likely we're in an --all-features session.
    // Since this is a common rust-analyzer configuration, allow it and fall back to
    // platform defaults.
    if backend_count > 1 && !all_features {
        panic!("Multiple crypto backends enabled, but only one can be selected");
    } else if backend_count == 0 || all_features {
        // Default to openssl on linux, the dependencies are also marked
        // non-optional and there is no native backend available
        if linux {
            println!("cargo::rustc-cfg=openssl");
        } else {
            println!("cargo::rustc-cfg=native");
        }
    }
    // Symcrypt does not support vendoring, so fail early if the user tries to
    // enable both the symcrypt backend and the vendored feature.
    else if vendored && symcrypt {
        panic!("The symcrypt backend does not support vendoring");
    }
    // Output a single config flag to indicate which backend should be used.
    else if openssl {
        println!("cargo::rustc-cfg=openssl");
    } else if symcrypt {
        println!("cargo::rustc-cfg=symcrypt");
    } else if rust {
        println!("cargo::rustc-cfg=rust");
    }
}
