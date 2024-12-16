// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_emulated_device;
mod fuzz_nvme_driver;

use crate::fuzz_nvme_driver::FuzzNvmeDriver;
use arbitrary::{Arbitrary, Unstructured};
use lazy_static::lazy_static;
use pal_async::DefaultPool;
use std::sync::Mutex;
use xtask_fuzz::fuzz_target;

// Input bytes we want to use
const INPUT_LEN:usize=4196;

// Use lazy_static to allow swapping out underlying vector
lazy_static! {
    pub static ref RAW_DATA: Mutex<Vec<u8>> = Mutex::new(Vec::new());
}

/** Uses static RAW_DATA to generate a vector of len=num_bytes with arbitrary bytes
*   
*  # Arguments
*  num_bytes: The number of bytes requested/size of return vector
*  
*  # Returns
*  - Ok(Vec<u8>) for success
*  - Err(..) for failure
*/
pub fn get_raw_data(num_bytes: usize) -> arbitrary::Result<Vec<u8>>{
    // Lock RAW_DATA before consuming
    let mut raw_data = RAW_DATA.lock().unwrap();

    // Case: Not enough bytes, return Error
    if raw_data.len() < num_bytes {
        println!("Not enough data in the backend anymore");
        return Err(arbitrary::Error::NotEnoughData);
    }

    // Consume bytes from RAW_DATA
    let drained: Vec<u8> = raw_data.drain(0..num_bytes).collect();
    return Ok(drained);
}

/// Guarantee a large arbitrary vector upon startup.
#[derive(Debug)]
pub struct LargeVec {
    pub vec: Vec<u8>
}

impl<'a> Arbitrary<'a> for LargeVec {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let mut vec = Vec::new();
        while vec.len() < INPUT_LEN {
            vec.push(u.arbitrary()?);
        }
        Ok(LargeVec {
            vec,
        })
    }   
}

/// Fuzzer loop. Loops while there is still raw data available to use.
fn do_fuzz() {
    // DefaultPool provides us the standard DefaultDriver and takes care of async fn calls
    DefaultPool::run_with(|driver| async move {
        // ---- SETUP ----
        let mut fuzzing_driver = FuzzNvmeDriver::new(driver).await;

        // ---- FUZZING ----
        loop {
            let next_action = fuzzing_driver.get_arbitrary_action();

            match next_action {
                Ok(action) => {  // Execute the Action
                    println!("{:x?}", action);
                    fuzzing_driver.execute_action(action);
                },
                Err(_e) => {  // Not Enough data, stop fuzzing
                    break;
                },
            }
        }

        // ---- CLEANUP ----
        // fuzzing_driver.shutdown().await;
    });
}

// Closure that allows the fuzzer to invoke the do_fuzz function on each iteration
fuzz_target!(|input: LargeVec| {
    xtask_fuzz::init_tracing_if_repro();

    // Swap out the underlying raw data in the RAW_DATA static variable.
    {
        let mut raw_data = RAW_DATA.lock().unwrap();
        *raw_data = input.vec;
    }

    do_fuzz();
});
