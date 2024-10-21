// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::uefi::splash;
use crate::uefi::Splashes;
use core::num::NonZeroU8;
use uefi::boot;
use uefi::cstr16;
use uefi::println;
use uefi::runtime;
use uefi::table::runtime::VariableVendor;

// TODO: add runtime config for which tests to run (e.g: skipping watchdog)
pub fn run_tests() {
    println!("running tests...");
    let mut splash_seq = 2u8;

    macro_rules! do_test {
        ($test_fn:ident) => {{
            let name = stringify!($test_fn);
            println!(">>>>>> [TEST]: running '{}'", name);
            $test_fn();
            splash_seq = splash_seq.wrapping_shl(1);
            splash::draw_splash(Splashes(NonZeroU8::new(splash_seq).unwrap()));
            boot::stall(1000000); // stall for 1 seconds
        }};
    }

    do_test!(test_global_alloc);

    do_test!(test_dbdefault);
    // TODO: re-enable when UEFI handles dbDefault correctly
    //do_test!(test_readonly);

    // leave the watchdog test for last, since it blows away the VM
    do_test!(test_watchdog);
}

fn test_global_alloc() {
    let s = format!("hello {}!", "world");
    println!("look 'ma, i'm outputting a heap allocated string: {}", s);
}

fn test_watchdog() {
    boot::set_watchdog_timer(5, 0xdeadbeef, None).unwrap();
    boot::stall(1000000 * 6); // stall for 6 seconds
    panic!("watchdog should've expired, but we're still running!")
}

fn test_dbdefault() {
    let db = runtime::get_variable_boxed(cstr16!("db"), &VariableVendor::IMAGE_SECURITY_DATABASE);
    let dbdefault =
        runtime::get_variable_boxed(cstr16!("dbDefault"), &VariableVendor::GLOBAL_VARIABLE);

    let (db_data, _) = db.expect("db not found");
    let (dbdefault_data, _) = dbdefault.expect("dbDefault not found");

    assert_eq!(db_data, dbdefault_data);
}

/* TODO: re-enable when UEFI handles dbDefault correctly
fn test_readonly(rt: &RuntimeServices) {
    match rt.set_variable(
        cstr16!("dbDefault"),
        &VariableVendor::GLOBAL_VARIABLE,
        VariableAttributes::BOOTSERVICE_ACCESS | VariableAttributes::RUNTIME_ACCESS,
        &[0, 1, 2],
    ) {
        Ok(_) => {
            panic!("Expected an error, but got Ok")
        }
        Err(error) => {
            assert_eq!(
                error.status(),
                Status::WRITE_PROTECTED,
                "Error status: {:?}",
                error.status()
            )
        }
    }
}
*/
