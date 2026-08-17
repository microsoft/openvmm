// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This is the main entrypoint for opentmk_invariant.
//!
//! Opentmk Invariant is a bare-bones operating system based off of opentmk
//! framework used to accept customly crafted program consisting of an encoded
//! series of functions that would invoke specific functions into this OS

#![cfg_attr(target_os = "uefi", no_main)]
#![cfg_attr(target_os = "uefi", no_std)]

mod comms;
mod deserializer;
mod executor;
mod functions;
mod prelude;
mod rt;

use crate::executor::Executor;

use opentmk_core::arch::serial::SerialPort;

#[cfg(target_os = "uefi")]
use uefi::println;

#[cfg(target_os = "uefi")]
#[uefi::entry]
fn uefi_entry() -> uefi::Status {
    _ = uefi::helpers::init();
    main();
    uefi::Status::ABORTED
}

fn main() {
    println!("Invariant OpenTMK kernel");

    #[cfg(target_os = "uefi")]
    {
        // Note: println() will no longer work after this step
        // since init() will exit boot services where enabled.
        // use log from henceforth for SERIAL port 2 logging
        match opentmk_core::uefi::init::init() {
            Ok(_) => log::info!("OpenTMK initialization complete!"),
            Err(e) => {
                log::info!("OpenTMK initialization failed! - {:?}", e);
                return;
            }
        };
    }

    let mut exec = Executor::new(SerialPort::COM1);

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
