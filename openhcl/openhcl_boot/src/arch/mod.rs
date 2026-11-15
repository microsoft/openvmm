// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Imports and re-exports architecture-specific implementations.

mod aarch64;
mod x86_64;

cfg_select! {
    target_arch = "x86_64" => {
        pub use x86_64::*;
    }
    target_arch = "aarch64" => {
        pub use aarch64::*;
    }
    _ => {
        compile_error!("target_arch is not supported");
    }
}
