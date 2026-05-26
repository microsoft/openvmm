// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_ALLOW_MULTIPLE_BACKENDS");
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

    let native = std::env::var_os("CARGO_FEATURE_NATIVE").is_some();
    let openssl = std::env::var_os("CARGO_FEATURE_OPENSSL").is_some();
    let rust = std::env::var_os("CARGO_FEATURE_RUST").is_some();
    let symcrypt = std::env::var_os("CARGO_FEATURE_SYMCRYPT").is_some();
    let vendored = std::env::var_os("CARGO_FEATURE_VENDORED").is_some();

    let allow_multiple_backends =
        std::env::var_os("CARGO_FEATURE_ALLOW_MULTIPLE_BACKENDS").is_some();

    let linux = std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "linux";

    let backend_count = openssl as u8 + rust as u8 + symcrypt as u8 + native as u8;

    let mut backend_list = Vec::new();
    if openssl {
        backend_list.push("openssl");
    }
    if symcrypt {
        backend_list.push("symcrypt");
    }
    if rust {
        backend_list.push("rust");
    }
    if native {
        backend_list.push("native");
    }
    let backend_list_str = backend_list.join(", ");

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
                panic!("Symcrypt does not support vendoring.");
            }
            println!("cargo::rustc-cfg=symcrypt");
        } else if rust {
            println!("cargo::rustc-cfg=rust");
        } else if native && !linux {
            println!("cargo::rustc-cfg=native");
        } else if native && linux {
            println!("cargo::rustc-cfg=openssl");
        }
    }
    // If we see multiple backends enabled that's an error. However if
    // allow-multiple-backends is enabled print a warning and allow it.
    else if allow_multiple_backends {
        println!(
            "cargo::warning=allow-multiple-backends is enabled, this may produce insecure binaries."
        );
        println!("cargo::warning=Backends enabled: {}", backend_list_str);

        if openssl {
            println!("cargo::warning=Using OpenSSL backend.");
            println!("cargo::rustc-cfg=openssl");
        } else if symcrypt && !vendored {
            println!("cargo::warning=Using Symcrypt backend.");
            println!("cargo::rustc-cfg=symcrypt");
        } else if rust {
            println!("cargo::warning=Using Rust backend.");
            println!("cargo::rustc-cfg=rust");
        } else if native && !linux {
            println!("cargo::warning=Using native backend.");
            println!("cargo::rustc-cfg=native");
        } else if native && linux {
            println!("cargo::warning=Using OpenSSL backend (native on Linux).");
            println!("cargo::rustc-cfg=openssl");
        }
    } else {
        println!("cargo::warning=Backends enabled: {}", backend_list_str);
        panic!(
            "Multiple crypto backends enabled. Please enable only one in your binary's dependencies."
        );
    }
}
