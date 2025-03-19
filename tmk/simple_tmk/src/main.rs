// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A simple test microkernel (TMK) for testing very basic VMM functionality.

#![cfg_attr(minimal_rt, no_std, no_main)]

#[cfg(minimal_rt)]
mod tmk;

#[cfg(not(minimal_rt))]
fn main() {
    unimplemented!("build with MINIMAL_RT_BUILD to produce a working binary");
}
