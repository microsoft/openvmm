// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Binary entry point for the standalone VMM.Perf runner.

fn main() -> anyhow::Result<()> {
    vmm_perf::run()
}
