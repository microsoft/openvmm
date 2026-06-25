// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

//! A 2-way fuzzer developed to fuzz the nvme driver from the Guest side with arbitrary driver
//! actions and from the Host side with arbitrary responses from the backend.
mod fuzz_emulated_device;
mod fuzz_nvme_driver;

use crate::fuzz_nvme_driver::FuzzNvmeDriver;

use arbitrary::Unstructured;
use pal_async::DefaultPool;
use xtask_fuzz::fuzz_target;

fn do_fuzz(u: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    DefaultPool::run_with(async |driver| {
        let mut fuzzing_driver = match FuzzNvmeDriver::new(driver, u).await {
            Ok(d) => d,
            Err(_) => return Err(arbitrary::Error::IncorrectFormat),
        };

        // Loop until we either run out of input or a driver call returns an
        // error; both indicate this iteration is done.
        while fuzzing_driver.execute_arbitrary_action(u).await.is_ok() {}

        fuzzing_driver.shutdown().await;
        Ok(())
    })
}

fuzz_target!(|input: &[u8]| -> libfuzzer_sys::Corpus {
    xtask_fuzz::init_tracing_if_repro();
    if do_fuzz(&mut Unstructured::new(input)).is_err() {
        libfuzzer_sys::Corpus::Reject
    } else {
        libfuzzer_sys::Corpus::Keep
    }
});
