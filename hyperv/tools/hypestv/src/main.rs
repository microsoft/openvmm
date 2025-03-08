// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interactive CLI for Hyper-V VMs.

#![warn(missing_docs)]

mod windows;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    pal_async::DefaultPool::run_with(windows::main)
}

#[cfg(not(windows))]
fn main() {
    eprintln!("not supported on this platform");
    std::process::exit(1);
}
