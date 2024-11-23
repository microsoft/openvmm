// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

#[derive(Debug, Arbitrary)]
struct FuzzInput {
    inputs: Vec<String>,
}

fn do_fuzz(input: FuzzInput) {
    fuzz_eprintln!("repro-ing test case...");

    // < fuzz code here >
    let _ = input.inputs;
}

fuzz_target!(|input: FuzzInput| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input)
});
