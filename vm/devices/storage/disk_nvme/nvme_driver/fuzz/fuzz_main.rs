// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

//! A 2-way fuzzer developed to fuzz the nvme driver from the Guest side with arbitrary driver
//! actions and from the Host side with arbitrary responses from the backend.
mod fuzz_emulated_device;
mod fuzz_nvme_driver;

use crate::fuzz_nvme_driver::FuzzNvmeDriver;

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use lazy_static::lazy_static;
use pal_async::DefaultPool;
use std::sync::Mutex;
use xtask_fuzz::fuzz_target;

// Use lazy_static to allow swapping out underlying vector
lazy_static! {
    pub static ref RAW_DATA: Mutex<Vec<u8>> = Mutex::new(Vec::new());
}

/// Consumes part of static RAW_DATA to generate a vector of len=num_bytes with arbitrary bytes
fn get_raw_data(num_bytes: usize) -> Result<Vec<u8>, arbitrary::Error>{
    let mut raw_data = RAW_DATA.lock().unwrap();

    // Case: Not enough data
    if raw_data.len() < num_bytes {
        return Err(arbitrary::Error::NotEnoughData);
    }

    let split = raw_data.len() - num_bytes;
    let consumed_data: Vec<u8> = raw_data.split_off(split);
    return Ok(consumed_data);
}

/// Returns an arbitrary data of type T or a NotEnoughData error. Generic type must
/// implement Arbitrary (for any lifetime 'a) and the Sized traits.
pub fn arbitrary_data<T>() -> Result<T, arbitrary::Error> 
where
for <'a> T: Arbitrary<'a> + Sized,
{
    let arbitrary_data = get_raw_data(size_of::<T>());

    let arbitrary_type = arbitrary_data.map(|data| -> T {
        let mut u = Unstructured::new(&data);

        let value: T = u.arbitrary().unwrap();
        return value;
    });

    return arbitrary_type;
}

/// Uses the provided input to repeatedly create and execute an arbitrary action on the NvmeDriver.
fn do_fuzz() {
    DefaultPool::run_with(|driver| async move {
        let mut fuzzing_driver = FuzzNvmeDriver::new(driver).await;

        loop {
            let next_action = fuzzing_driver.execute_arbitrary_action().await;

            // Not enough data
            if let Err(_e) = next_action {
                break;
            }
        }

        fuzzing_driver.shutdown().await;
    });
}

fuzz_target!(|input: Vec<u8>| {
    xtask_fuzz::init_tracing_if_repro();

    // Swap out the underlying raw data.
    {
        let mut raw_data = RAW_DATA.lock().unwrap();
        *raw_data = input;
    }

    do_fuzz();
});
