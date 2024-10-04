// Copyright (C) Microsoft Corporation. All rights reserved.

/// Initializes compiler flags for building a minimal kernel using the
/// `minimal_rt` crate.
///
/// Only does anything if the `MINIMAL_RT_BUILD` environment variable is set.
///
/// Also, sets the `minimal_rt` `cfg` so that code can detect that it should
/// build for running as a minimal kernel.
pub fn init() {
    println!("cargo:rustc-check-cfg=cfg(minimal_rt)");

    // If the user sets this environment variable, build the binary for use as a
    // boot loader. Otherwise, just build a stub binary for unit tests, clippy,
    // rust-analyzer, etc.
    //
    // We don't use a feature for this because because this would break
    // `--all-features`. We don't use a profile or something for this because
    // cargo doesn't want us to know about custom profiles. There's no other
    // mechanism I know of to communicate this information through cargo.
    println!("cargo:rerun-if-env-changed=MINIMAL_RT_BUILD");
    if matches!(
        std::env::var("MINIMAL_RT_BUILD").as_deref().ok(),
        None | Some("")
    ) {
        return;
    }

    let supported_triple;
    // xtask-fmt allow-target-arch sys-crate
    match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "x86_64" => {
            // This is the supported triple for x86-64 because, compared to a
            // linux target, it disables the red zone (needed to prevent
            // interrupts from overwriting the stack) and it disables use of SSE
            // by default (needed to avoid accidentally using SSE intrinstics in
            // various places).
            //
            // No special linker flags are needed.
            supported_triple = "x86_64-unknown-none";
        }
        "aarch64" => {
            // This is the supported triple for aarch64. The -none target does
            // not enable PIE, which we require so that the boot loader can run
            // anywhere in PA space.
            supported_triple = "aarch64-unknown-linux-musl";
            // Don't include the _start entry point.
            println!("cargo:rustc-link-arg=-nostartfiles");
            // Make the executable relocatable.
            println!("cargo:rustc-link-arg=-static-pie");
        }
        arch => panic!("unsupported arch {arch}"),
    }

    let triple = std::env::var("TARGET").unwrap();
    if triple != supported_triple {
        panic!(
            "build is only supported with the {} target, clear MINIMAL_RT_BUILD",
            supported_triple
        );
    }

    println!("cargo:rustc-cfg=minimal_rt");
}
