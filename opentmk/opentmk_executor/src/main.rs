// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This is the main entrypoint for opentmk_executor.
//!
//! Opentmk Executor is a bare-bones operating system based off of opentmk
//! framework used to accept custom-crafted program consisting of an encoded
//! series of functions that would invoke specific functions into this OS

#![cfg(any(test, target_os = "uefi"))]
#![cfg_attr(target_os = "uefi", no_main)]
#![no_std]

mod comms;
mod deserializer;
mod executor;
mod functions;
mod prelude;
#[cfg(target_os = "uefi")]
mod rt;
mod serial;

#[macro_use]
extern crate alloc;

#[cfg(not(target_os = "uefi"))]
extern crate std;

use opentmk_core::arch::serial::SerialPort;

#[cfg(target_os = "uefi")]
#[uefi::entry]
fn uefi_entry() -> uefi::Status {
    _ = uefi::helpers::init();

    uefi::println!("OpenTMK executor kernel");

    // Note: println() will no longer work after this step
    // since init() will exit boot services where enabled.
    // use log from henceforth for SERIAL port 2 logging
    match opentmk_core::uefi::init::init() {
        Ok(_) => {
            log::info!("OpenTMK initialization complete!");
            main();
        }
        Err(e) => {
            log::info!("OpenTMK initialization failed! - {:?}", e);
        }
    }

    uefi::Status::ABORTED
}

fn main() {
    let mut exec = executor::Executor::new(SerialPort::COM1);

    if let Err(e) = exec.initialize() {
        //TODO: we want to be able to catch these errors on the host side
        log::error!("Executor initialization failed with error {:?}", e);
        return;
    }

    exec.register_fuzz_functions();

    if let Err(e) = exec.run() {
        //TODO: we want to be able to catch these errors on the host side
        log::error!("Executor exited with an error - {:?}", e);
    }
}
