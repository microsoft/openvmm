// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_lib; 

use arbitrary::Arbitrary;
use pal_async::DefaultPool;
use xtask_fuzz::fuzz_target;

/// Struct for input from the fuzzer
/// * `data` - Array of arbitrary bytes
/// * `lba` - Logical Block Address to read from
/// * `block_count` - Number of block to read in
/// * `action` - Dictates the action that needs to be performed. 1 = Read, 2 = Write
#[derive(Debug, Arbitrary)]
struct FuzzInput {
    lba: u64,
    block_count: u32,
    action: usize
}

/// Writes the given arbitrary bytes to disk and reads arbitrary number of blocks from an arbitrary
/// address in the disk. The number of blocks being read can be larger than the provided memory.
///
/// # Arguments
/// * `input` - An arbitrary(see function above) FuzzInput struct
fn do_fuzz(input: FuzzInput) {
    // DefaultPool provides us the standard DefaultDriver and takes care of async fn calls
    DefaultPool::run_with(|driver| async move {
        // ---- SETUP ----
        let fuzzing_driver = fuzz_lib::FuzzNvmeDriver::new(driver).await;

        // ---- FUZZING ----
        // fuzzing_driver.read_arbitrary(input.lba, input.block_count).await;

        // ---- CLEANUP ----
        fuzzing_driver.shutdown().await;
    });
}

// Closure that allows the fuzzer to call the do_fuzz function.
fuzz_target!(|input: FuzzInput| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input)
});
