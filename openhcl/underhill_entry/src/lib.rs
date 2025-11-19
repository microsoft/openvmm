// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The entry point for the underhill environment.

#![forbid(unsafe_code)]
#![cfg(target_os = "linux")]

// Use mimalloc instead of the system malloc for performance.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// musl's memcpy implementation is slow on x86_64, so we use memcpy crate to
// provide an optimized implementation.
//
// xtask-fmt allow-target-arch sys-crate
#[cfg(target_arch = "x86_64")]
use fast_memcpy as _;

// OpenVMM-HCL only needs libcrypto from openssl, not libssl.
#[cfg(target_os = "linux")]
openssl_crypto_only::openssl_crypto_only!();

/// Entry point into the underhill multi-binary, dispatching between various
/// entrypoints based on argv0.
pub fn underhill_main() -> anyhow::Result<()> {
    let argv0 = std::path::PathBuf::from(std::env::args_os().next().unwrap());
    match argv0.file_name().unwrap().to_str().unwrap() {
        "underhill-init" => underhill_init::main(),
        "underhill-crash" => underhill_crash::main(),
        "underhill-dump" => underhill_dump::main(),
        _ => underhill_core::main(),
    }
}
