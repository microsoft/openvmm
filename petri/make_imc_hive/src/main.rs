// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows helper for creating the registry hive used to inject Pipette into
//! an isolated-memory test guest.
//!
//! The generated hive configures the Pipette Windows service and related guest
//! settings consumed while preparing VMM-test images. This binary runs on the
//! host and is normally invoked as an artifact by the `prep_steps`/Flowey test
//! pipeline, not as an interactive tool. Non-Windows builds provide only an
//! unsupported-platform stub.

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
