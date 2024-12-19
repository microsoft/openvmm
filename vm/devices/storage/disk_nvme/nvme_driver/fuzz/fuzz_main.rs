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
use pal_async::DefaultPool;
use std::sync::Mutex;
use xtask_fuzz::fuzz_target;

// Anything consumed by EmulatedDeviceFuzzer needs to be static because of DeviceBacking trait.
pub static RAW_DATA: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Returns an arbitrary data of type T or a NotEnoughData error. Generic type must
/// implement Arbitrary (for any lifetime 'a) and the Sized traits.
pub fn arbitrary_data<T>() -> Result<T, arbitrary::Error> 
where
for <'a> T: Arbitrary<'a> + Sized,
{
    let mut raw_data = RAW_DATA.lock().unwrap();
    let input = raw_data.split_off(0);
    let mut u = Unstructured::new(&input);

    if u.is_empty() {
        return Err(arbitrary::Error::NotEnoughData);
    }

    // If bytes needed is more than remaining bytes it will pad with 0s.
    let arbitrary_type: T = u.arbitrary()?;

    let x = u.take_rest().to_vec();
    *raw_data = x; 
    return Ok(arbitrary_type);
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
