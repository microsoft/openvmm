// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows-only interactive shell for developing and debugging Hyper-V VMs.
//!
//! hypestv can select a VM, change its power state, attach serial consoles, and
//! inspect or reload an OpenHCL paravisor. It combines those operations in a
//! REPL with history and completion and is intended for direct developer use,
//! not stable automation.
//!
//! Run `hypestv` to start detached and select a VM interactively, or pass a VM
//! name to select it at startup. Non-Windows builds provide only an
//! unsupported-platform stub.

#![forbid(unsafe_code)]

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
