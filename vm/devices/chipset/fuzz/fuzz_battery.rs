// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Unstructured;
use chipset::battery::BatteryDevice;
use chipset::battery::BatteryRuntimeDeps;
use chipset::battery::BATTERY_MMIO_REGION_BASE_ADDRESS_X64;
use chipset_resources::battery::HostBatteryUpdate;
use vmcore::line_interrupt::LineInterrupt;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

fn do_fuzz(u: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    // create battery dependencies
    let (tx, rx) = mesh::channel::<HostBatteryUpdate>();
    tx.send(u.arbitrary::<HostBatteryUpdate>()?);
    let notify_interrupt = LineInterrupt::detached();

    // create battery device, then add it to chipset
    let battery = BatteryDevice::new(
        BatteryRuntimeDeps {
            battery_status_recv: rx,
            notify_interrupt,
        },
        BATTERY_MMIO_REGION_BASE_ADDRESS_X64,
    );
    let mut chipset = chipset_device_fuzz::FuzzChipset::default();
    chipset.device_builder("battery").add(|_| battery).unwrap();

    while !u.is_empty() {
        let action = chipset.get_arbitrary_action(u)?;
        fuzz_eprintln!("{:x?}", action);
        chipset.exec_action(action).unwrap();

        // occasionally send new battery state
        if u.ratio(1, 5)? {
            tx.send(u.arbitrary::<HostBatteryUpdate>()?);
        }
    }

    Ok(())
}

fuzz_target!(|input: &[u8]| -> libfuzzer_sys::Corpus {
    xtask_fuzz::init_tracing_if_repro();
    if do_fuzz(&mut Unstructured::new(input)).is_err() {
        libfuzzer_sys::Corpus::Reject
    } else {
        libfuzzer_sys::Corpus::Keep
    }
});
