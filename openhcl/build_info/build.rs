// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    // Generate git information
    vergen::EmitBuilder::builder().all_git().emit().unwrap();
    
    // Set target architecture for const context
    if let Ok(target_arch) = std::env::var("CARGO_CFG_TARGET_ARCH") {
        println!("cargo:rustc-env=OPENVMM_BUILD_TARGET_ARCH={}", target_arch);
    }
}
