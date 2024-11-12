// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod rt;
mod splash;
mod tests;

use core::num::NonZeroU8;
use splash::Splashes;
use uefi::entry;
use uefi::println;
use uefi::system;
use uefi::Status;

#[entry]
fn uefi_main() -> Status {
    println!("UEFI vendor = {}", system::firmware_vendor());
    println!("UEFI revision = {:x}", system::firmware_revision());

    // Attempt to draw a pretty splash screen. Not always possible (e.g: when
    // running UEFI on a VM without a gfx adapter - such as in CI).
    splash::draw_splash(Splashes(NonZeroU8::new(1).unwrap()));

    tests::run_tests();

    Status::SUCCESS
}
