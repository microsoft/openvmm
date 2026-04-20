// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    // Allow a cfg of nightly to avoid using a feature, see main.rs.
    println!("cargo:rustc-check-cfg=cfg(nightly)");

    // By default the sha2 crate uses cpu feature detection which on x86_64 uses the
    // cpuid instruction. Executing cpuid in an SNP CVM would require implementing an
    // exception handler. Using the "soft" configuration flag forces a software
    // implementation of the hashing algorithms that does not use cpuid. The
    // "compact" flag disables loop unrolling, providing a smaller code size at
    // the cost of performance.
    println!("cargo:rustc-cfg=sha2_backend=\"soft\"");
    println!("cargo:rustc-cfg=sha2_backend_soft=\"compact\"");

    minimal_rt_build::init();
}
