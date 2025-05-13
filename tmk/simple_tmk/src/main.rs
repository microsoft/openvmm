// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A simple test microkernel (TMK) for testing very basic VMM functionality.

#![cfg_attr(minimal_rt, no_std, no_main)]
// UNSAFETY: TMK tests are going to need to perform unsafe operations.
#![allow(unsafe_code)]

mod prelude;

mod aarch64;
mod common;
mod x86_64;

#[cfg(not(minimal_rt))]
fn main() {
    unimplemented!("build with MINIMAL_RT_BUILD to produce a working binary");
}
