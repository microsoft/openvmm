// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_lib; 

use arbitrary::Unstructured;
use pal_async::DefaultPool;
use xtask_fuzz::fuzz_target;

/// Writes the given arbitrary bytes to disk and reads arbitrary number of blocks from an arbitrary
/// address in the disk. The number of blocks being read can be larger than the provided memory.
///
/// TODO
fn do_fuzz(u: &mut Unstructured<'_>) {
    // DefaultPool provides us the standard DefaultDriver and takes care of async fn calls
    DefaultPool::run_with(|driver| async move {
        // ---- SETUP ----
        let fuzzing_driver = fuzz_lib::FuzzNvmeDriver::new(driver).await;

        // ---- FUZZING ----
        while !u.is_empty() {
            let next_action = fuzzing_driver.get_arbitrary_action(u).unwrap();

            // println!("{:x?}", next_action);
            fuzzing_driver.execute_action(next_action);
        }

        // ---- CLEANUP ----
        fuzzing_driver.shutdown().await;
    });
}

// Closure that allows the fuzzer to call the do_fuzz function.
// TODO: Do I need to implement something with the corpus here? Seems like the corpus here would
// only indicate length of the input that is passed in which doesn't really make too much sense
fuzz_target!(|input: &[u8]| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(&mut Unstructured::new(input))
});
