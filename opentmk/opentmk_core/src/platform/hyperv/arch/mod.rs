// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V platform architecture-specific modules.

pub mod hypercall;

cfg_select! {
    target_arch = "x86_64" => {
        mod x86_64;
        pub use x86_64::*;
    }
    target_arch = "aarch64" => {
        mod aarch64;
        pub use aarch64::*;
    }
    _ => {
        compile_error!("target_arch is not supported");
    }
}
