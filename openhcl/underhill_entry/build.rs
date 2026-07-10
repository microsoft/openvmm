// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Points rustc at the prebuilt mimalloc static library shipped in the
//! openvmm-deps sdk sysroot, picking between the non-secure and
//! `MI_SECURE=4` flavors based on whether the `mi-secure` cargo feature is
//! enabled.
//!
//! The matching `[target.<musl-triple>.mimalloc]` stub overrides in
//! `.cargo/config.toml` suppress `libmimalloc-sys`'s real build script (so
//! mimalloc is not recompiled from source on every `cargo build`). Cargo
//! `links` overrides cannot depend on cargo features, however, so the
//! actual `-l` / `-L` directives are emitted here.
//!
//! On non-musl targets we leave linking entirely to `libmimalloc-sys`'s own
//! build script.

use std::path::PathBuf;

fn main() {
    // xtask-fmt allow-target-arch sys-crate
    println!("cargo::rerun-if-env-changed=CARGO_CFG_TARGET_ARCH");
    println!("cargo::rerun-if-env-changed=CARGO_CFG_TARGET_ENV");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_MI_SECURE");
    println!("cargo::rerun-if-env-changed=CARGO_MANIFEST_DIR");

    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env != "musl" {
        return;
    }

    // xtask-fmt allow-target-arch sys-crate
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
    let sysroot_lib = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent() // openhcl/
        .and_then(|p| p.parent()) // workspace root
        .unwrap()
        .join(".packages")
        .join("extracted")
        .join(format!("{arch}-sysroot"))
        .join("lib");

    let lib = if std::env::var_os("CARGO_FEATURE_MI_SECURE").is_some() {
        "mimalloc-secure"
    } else {
        "mimalloc"
    };

    println!("cargo:rustc-link-search=native={}", sysroot_lib.display());
    println!("cargo:rustc-link-lib=static={lib}");
}
