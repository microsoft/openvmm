// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host-side entry point for the standalone VMM.Perf runner.
//!
//! The runner executes packaged VirtualClient profiles against an OpenVMM
//! executable, collecting metrics and diagnostic artifacts for each selected
//! VM configuration. It supports native Linux and Windows hosts.
//!
//! Developers normally use `cargo xflowey vmm-perf`, which builds the runner
//! and supplies OpenVMM, UEFI firmware, and the pinned runtime archive. Direct
//! invocation is supported when those artifact paths are passed explicitly;
//! run `vmm_perf --help` for the required inputs.

fn main() -> anyhow::Result<()> {
    vmm_perf::run()
}
