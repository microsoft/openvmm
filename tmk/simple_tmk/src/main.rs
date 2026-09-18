// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bare-metal Test Microkernel (TMK) payload for low-level VMM tests.
//!
//! The binary contains small architecture-specific tests registered in the
//! ELF `tmk_tests` section. [`tmk_vmm`](https://docs.rs/tmk_vmm) discovers
//! those descriptors, loads this image as a guest kernel, and runs each test
//! without booting firmware or a general-purpose operating system.
//!
//! This is not a host CLI. It must be built with the repository's
//! `minimal_rt` configuration and is normally built and consumed by the TMK
//! portions of `cargo xflowey vmm-tests-run`.

#![cfg_attr(minimal_rt, no_std, no_main)]
// UNSAFETY: TMK tests are going to need to perform unsafe operations.
#![cfg_attr(target_arch = "x86_64", expect(unsafe_code))]

mod prelude;

mod aarch64;
mod common;
mod x86_64;

#[cfg(not(minimal_rt))]
fn main() {
    unimplemented!("build with MINIMAL_RT_BUILD to produce a working binary");
}
