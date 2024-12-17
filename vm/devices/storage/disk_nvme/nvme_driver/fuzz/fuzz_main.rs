// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

//! A 2-way fuzzer developed to fuzz the nvme driver from the Guest side with arbitrary driver
//! action and from the Host side with arbitrary responses from the backend.
mod fuzz_emulated_device;
mod fuzz_nvme_driver;

use crate::fuzz_nvme_driver::FuzzNvmeDriver;

use arbitrary::Unstructured;
use lazy_static::lazy_static;
use pal_async::DefaultPool;
use std::sync::Mutex;
use xtask_fuzz::fuzz_target;

// Use lazy_static to allow swapping out underlying vector
lazy_static! {
    pub static ref RAW_DATA: Mutex<Vec<u8>> = Mutex::new(Vec::new());
}

/// Uses static RAW_DATA to generate a vector of len=num_bytes with arbitrary bytes
pub fn get_raw_data(num_bytes: usize) -> arbitrary::Result<Vec<u8>>{
    // Lock RAW_DATA before consuming
    let mut raw_data = RAW_DATA.lock().unwrap();

    // Case: Not enough bytes, return Error
    if raw_data.len() < num_bytes {
        return Err(arbitrary::Error::NotEnoughData);
    }

    // Consume bytes from RAW_DATA
    let drained: Vec<u8> = raw_data.drain(0..num_bytes).collect();
    return Ok(drained);
}


/// Returns an arbitrary boolean value. If there isn't enough data, returns false
pub fn arbitrary_bool() -> bool {
    // Get required number of arbitrary bytes
    let arbitrary_data = get_raw_data(size_of::<bool>());

    // Generate an arbitrary boolean value
    match arbitrary_data {
        Ok(data) => {
            let mut u = Unstructured::new(&data);

            // Generate arbitrary action
            let result = u.arbitrary();

            match result {
                Ok(arbitrary_bool) => { return arbitrary_bool; }
                Err(_e) => {}
            } 
        }
        Err(_e) => {}
    }

    // In case of errors, default to false
    return false;
}

/// Fuzzer loop. Loops while there is still raw data available to use.
fn do_fuzz() {
    // DefaultPool provides us the standard DefaultDriver and takes care of async fn calls
    DefaultPool::run_with(|driver| async move {
        // Setup
        let mut fuzzing_driver = FuzzNvmeDriver::new(driver).await;

        // While arbitrary data is not empty, keep fuzzing.
        loop {
            let next_action = fuzzing_driver.get_arbitrary_action();

            match next_action {
                Ok(action) => {
                    fuzzing_driver.execute_action(action).await;
                },
                Err(_e) => {
                    break;
                },
            }
        }

        // Cleanup
        fuzzing_driver.shutdown().await;
    });
}

// Closure that allows the fuzzer to invoke the nvme driver fuzzer.
fuzz_target!(|input: Vec<u8>| -> libfuzzer_sys::Corpus {
    xtask_fuzz::init_tracing_if_repro();

    // Swap out the underlying raw data.
    {
        let mut raw_data = RAW_DATA.lock().unwrap();
        *raw_data = input;
    }

    do_fuzz();
    libfuzzer_sys::Corpus::Keep
});
