// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tool to make an IMC hive for injecting pipette into a Windows guest.


#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
fn main() {
    eprintln!("not supported on this OS");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows::main()
}
