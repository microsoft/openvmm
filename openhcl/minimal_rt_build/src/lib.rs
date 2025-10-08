// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

/// Initializes compiler flags for building a minimal kernel using the
/// `minimal_rt` crate.
///
/// Only does anything if the `MINIMAL_RT_BUILD` environment variable is set.
///
/// Also, sets the `minimal_rt` `cfg` so that code can detect that it should
/// build for running as a minimal kernel.
pub fn init() -> bool {
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
        return false;
    }

    let triple = std::env::var("TARGET").unwrap();
    let unsupported = |supported_triple| {
        panic!(
            "build is only supported with the {} target, not {}, clear MINIMAL_RT_BUILD",
            supported_triple, triple
        );
    };
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
            if triple != "x86_64-unknown-none" {
                unsupported("x86_64-unknown-none");
            }
        }
        "aarch64" => {
            match triple.as_str() {
                "aarch64-minimal_rt-none" => {
                    // This is a custom target, defined via
                    // aarch64-minimal_rt-none.json. So, it requires
                    // RUSTC_BOOTSTRAP=1 or an unstable toolchain in order to
                    // use `-Zbuild-std`.
                    //
                    // It is aarch64-unknown-none with support for static PIE
                    // binaries, which we need to support loading the image
                    // anywhere in PA space.
                }
                "aarch64-unknown-linux-musl" => {
                    // This target works (it supports static PIE binaries) and
                    // does not require an unstable toolchain, but it is
                    // difficult to build from non-Linux host environments.
                    //
                    // This does require some tweaks to the linker flags.
                    //
                    // Don't include the _start entry point.
                    println!("cargo:rustc-link-arg=-nostartfiles");
                    // Make the executable relocatable.
                    println!("cargo:rustc-link-arg=-static-pie");
                }
                _ => {
                    unsupported("aarch64-unknown-linux-musl");
                }
            }
        }
        arch => panic!("unsupported arch {arch}"),
    }

    println!("cargo:rustc-cfg=minimal_rt");
    true
}
