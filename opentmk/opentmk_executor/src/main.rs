// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI execution payload for host-generated OpenTMK programs.
//!
//! The executor is a bare-bones operating system built on the OpenTMK runtime.
//! It receives framed packets over COM1, deserializes an encoded sequence of
//! function calls, and dispatches registered low-level operations such as port
//! I/O and hypercalls. This supports external fuzzing and hardware-interface
//! test drivers without booting a general-purpose guest OS.
//!
//! This binary is built as a UEFI guest payload and driven through its serial
//! protocol; it has no host-side command-line interface.

#![cfg_attr(target_os = "uefi", no_main)]
#![cfg_attr(target_os = "uefi", no_std)]

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

#[cfg(target_os = "uefi")]
#[uefi::entry]
fn uefi_entry() -> uefi::Status {
    main();
    uefi::Status::ABORTED
}

fn main() {
    #[cfg(target_os = "uefi")]
    {
        use uefi::println;

        _ = uefi::helpers::init();

        println!("OpenTMK executor kernel");

        // Note: println() will no longer work after this step
        // since init() will exit boot services where enabled.
        // use log from henceforth for SERIAL port 2 logging
        match opentmk_core::uefi::init::init() {
            Ok(_) => log::info!("OpenTMK initialization complete!"),
            Err(e) => {
                log::info!("OpenTMK initialization failed! - {:?}", e);
                return;
            }
        }
    }

    use opentmk_core::arch::serial::SerialPort;

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
